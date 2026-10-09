# NOAA GFS selective Zarr ingest

NOAA/NCEP の GFS 0.25° 全球予報から必要なGRIBメッセージだけをHTTP Rangeで取得し、
Rustでデコード後にZarr v2へ変換してS3互換ストレージへ保存するコンテナとCronJobです。
元の約500 MiB/ファイルのGRIB2全体はダウンロードも保存もしません。

全球Zarrの保存後、1日前のサイクルが保存済みなら2日分を連続した48時間系列として結合し、
気象庁MSMに近い日本周辺領域をGISの第3次地域メッシュ（約1km）の中心点へ双線形補間した
Zarrも作成します。

## 要素選択CSV

[`gfs-fields.csv`](gfs-fields.csv) は、GFS f003 の `.idx` に掲載された全743レコードを
1行ずつ収録しています。先頭が `#` のデータ行は無効です。取得したい行の `#` を削除し、
不要になった行へ `#` を追加して管理します。`##` で始まる行は説明コメントです。

既定で有効な要素は次の7種類です。

- 海面更正気圧
- 地上2m気温
- 地上2m相対湿度
- 地上10m東西風・南北風
- 総降水量
- 全雲量

CSV列の意味:

| 列 | 用途 |
|---|---|
| `short_name` | GRIB要素略号。例: `TMP`, `UGRD` |
| `level` | 鉛直面・層。例: `2 m above ground` |
| `statistic` | `instant`, `accumulation`, `average`, `maximum`, `minimum` |
| `occurrence` | 同じ要素・面・統計が複数ある場合の出現順 |
| `zarr_name` | Zarr配列名。小文字英数字とunderscoreを使用 |
| `common_name_ja` | 要素の日本語通称 |

実行のたびに各予報時刻の小さな `.idx` を取得し、現在の開始・終了byteを計算します。
このため、日付や予報時刻によるGRIBサイズの変化に追従します。

CSVを作り直す場合は、代表となるf003のIDXを指定します。再生成するとコメント選択も既定値へ
戻るため、既存CSVとの差分を確認してください。

```sh
gfs_url=https://nomads.ncep.noaa.gov/pub/data/nccf/com/gfs/prod/gfs.YYYYMMDD/HH/atmos/gfs.tHHz.pgrb2.0p25.f003
curl -o /tmp/gfs.idx "${gfs_url}.idx"
cargo run --bin generate-catalog -- \
  /tmp/gfs.idx "${gfs_url}.idx" > gfs-fields.csv
```

## 取得・変換処理

1. 実行時刻から24時間前をGFSの6時間サイクルへ切り下げます。
2. 各 `GFS_FORECAST_HOURS` の `.idx` を取得します。
3. CSVで有効なレコードのbyte範囲だけを `Range: bytes=start-end` で取得します。
4. サーバーがHTTP 206を返さない場合は、全量取得を避けるため失敗させます。
5. ecCodesで各メッセージをデコードし、全球721×1440格子をZarr v2へ格納します。
6. ZarrをRustFSへアップロードし、全オブジェクト成功後に `_SUCCESS` を作成します。
7. 1日前の全球Zarrに `_SUCCESS` があれば、前日f000-f024と直近f003-f024を結合します。
8. 日本周辺領域の補間に必要な周囲の格子点を含むZarrチャンクだけをRustFSから読み、
   第3次地域メッシュの中心点へ双線形補間して日本周辺Zarrを保存します。

新規データを保存する前に保持期限の削除を実行します。既定の `RETENTION_DAYS=7` では、
対象初期時刻のちょうど7日前およびそれ以前の全球Zarrと日本周辺Zarrを削除します。
移行時に古いデータが残らないよう、旧形式の `noaa/gfs` と `forecast/gfs` も削除対象として
認識します。管理対象の日付形式に一致しないオブジェクトは削除しません。削除処理が
失敗した場合は新規データを保存せず、Jobを失敗させます。

CronJobは毎日 `04:15, 10:15, 16:15, 22:15 UTC` に実行します。24時間前の時刻を
6時間単位へ切り下げるため、それぞれ前日の `00Z, 06Z, 12Z, 18Z` が対象になります。

既定の予報時刻は `f000, f003, ..., f024` です。f000に存在しない積算降水量などは
Zarr上でNaNとなり、f003以降に値が入ります。領域切り出しは行わず全球を保持します。

保存先は次の形式です。

```text
s3://weather/noaa-gfs/YYYYMMDDHH.zarr/
```

配列の次元は `(forecast_hour, latitude, longitude)` です。Zstd圧縮とbit-shuffleを使い、
Xarrayから読めるよう各配列に `_ARRAY_DIMENSIONS` を設定し、メタデータをconsolidateします。

## 48時間日本周辺予報

気象庁の[メソ数値予報モデルGPV仕様](https://www.data.jma.go.jp/suishin/cgi-bin/catalogue/make_product_page.cgi?id=MesModel)と
同じ北緯22.4～47.6度、東経120～150度を領域の外周境界に使用します。
[第3次地域メッシュの定義](https://www.stat.go.jp/data/mesh/m_tuite.htm)に合わせ、
緯度30秒（1/120度）×経度45秒（1/80度）の各セル中心へGFSの値を双線形補間します。
既定の格子数は緯度3024×経度2400です。緯度・経度は丸めによるメッシュのずれを避けるため
`float64` で保存し、セル境界を表すbounds配列も付与します。緯度の中心は
北緯22.404166…～47.595833…度、経度の中心は東経120.00625～149.99375度です。
中心座標と第3次地域メッシュの規則から8桁のメッシュコードを計算できます。

補間に寄与する格子点に欠損値があれば結果もNaNとし、重みがゼロの格子点の欠損値は
結果へ伝播させません。元データはGFSの0.25度格子です。約1kmの格子に保存しても、
地形による気温差や局地的な降雨などの物理情報が増えるわけではありません。

前日サイクルの `f000, f003, ..., f024` と直近サイクルの `f003, ..., f024` を結合し、
前日サイクルを基準とする `forecast_hour=0, 3, ..., 48` の17時刻を作ります。同じ有効時刻に
なる直近サイクルのf000は除外し、前日サイクルのf024を採用します。

各気象変数の配列shapeは `[17, 3024, 2400]`、チャンクshapeは `[1, 512, 512]` です。
領域を小さくした場合、空間チャンクの各次元は `min(512, その次元の格子数)` になります。
気象変数は `float32` のまま保存するため、既定の7変数×17時刻で非圧縮データ量は
約3.22GiB/サイクルです。Zstd圧縮後の保存量は気象場によって変動します。

生成したチャンクファイルは1時刻分ずつアップロードし、成功したローカルファイルを
削除します。全時刻のアップロード後に最終メタデータと `_SUCCESS` を保存するため、
48時間分の高解像度Zarr全体をローカルに置く必要はありません。CronJobの一時領域は
既存の2Giのままです。利用側は `_SUCCESS` があるZarrだけを完成済みとして扱ってください。

保存先:

```text
s3://weather/forecast/YYYYMMDDHH.zarr/
```

`YYYYMMDDHH` は48時間系列の起点となる前日サイクルです。前日または直近の全球Zarrに
`_SUCCESS` がなければ成功扱いでスキップします（`Ok(())`）。入力不足だけではJobの
失敗リトライは発生しません。両方の入力が揃った後、同じ対象サイクルが再選択される
実行でのみ再試行します。次の6時間後の定期実行では対象サイクルが進むため、
過去の未生成サイクルは自動補完されません。対象サイクルは実行時刻と現在の設定で
決まり、手動実行でも保持期限の削除が先に実行されます。

`FORECAST_ENABLED=false` で
後続処理を無効化でき、境界と保存prefixは `FORECAST_LAT_*`, `FORECAST_LON_*`,
`FORECAST_PREFIX` で変更できます。

5.0.0では `_SUCCESS` の `processing` 記述を照合して処理済みか判定します。
同じ保存キーに旧101×121格子の日本周辺Zarrがある場合、記述が一致しないため初回に
再生成します。このとき保存済みの全球Zarrを利用し、NOAAから全球データを再取得しません。
再生成前に、その予報サイクルの出力ディレクトリだけを消去してから書き直すため、
除外した変数や失敗した試行のチャンクが残りません。再生成中は `_SUCCESS` がありません。
実行対象のサイクルだけを処理するため、過去の全サイクルを自動で再生成することはありません。

### 気象庁データの調査と補間方法

[メッシュ平年値2020](https://www.data.jma.go.jp/stats/etrn/view/atlas.html)は、
[国土数値情報のGML・Shapefile](https://nlftp.mlit.go.jp/ksj/gml/datalist/KsjTmplt-G02-v3_0.html)で
取得できます。ただし、公開配布版は月・年の気候平均で、湿度・風・海面気圧を含みません。
この資料だけを時刻別の7要素へ直接適用することはできません。
[推計気象分布](https://www.data.jma.go.jp/developer/weatherdataguide/appendix/1-1-b.html)も
約1kmの実況資料で、48時間先の予報を提供するものではありません。

MSM・[LFMのGPV](https://www.data.jma.go.jp/suishin/cgi-bin/catalogue/make_product_page.cgi?id=KyoModel)は
気象業務支援センター経由で即時配信され、
[気象庁の公開ダウンロード](https://www.data.jma.go.jp/developer/gpv_sample.html)は固定サンプルです。
LFMの予報は最長18時間、MSMは00・12UTC以外のサイクルでは39時間です。
現行サービスに継続取得できる気象庁GPVの配信設定はないため、今回はGFSの双線形補間を採用します。

## ビルドとデプロイ

RustFSに `weather` bucketを作成し、`default` Namespaceの既存の
`rustfs-credentials` Secret（`RUSTFS_ACCESS_KEY`, `RUSTFS_SECRET_KEY`）を利用します。
認証情報はリポジトリ、イメージ、ConfigMapへ含めません。

`.github/workflows/publish-image.yml` はPRでイメージをビルドし、`main` へのpushでは
`ghcr.io/uiui611/noaa-gfs-ingest:main` とコミットSHAのタグをGHCRへ公開します。
公開後、GitHub OIDCトークンで `https://deploy.mizu-mizu.info/v1/deployments` に
`noaa-gfs-ingest` の更新を通知します。通知に失敗してもイメージ公開は成功扱いです。

初回公開後はGHCRパッケージをpublicにし、受信側へリポジトリのOIDC許可設定と
`default/noaa-gfs-ingest` CronJobへのpatch権限を登録してください。
GitHub Actionsは `GITHUB_TOKEN` で公開するため、追加の長期トークンは不要です。

```sh
kubectl apply --dry-run=server -n default -f noaa-gfs-cronjob.yaml
kubectl apply -n default -f noaa-gfs-cronjob.yaml
```

CronJobは `main` タグを `Always` で取得します。webhookはPodテンプレートのannotationを
更新し、次の定期実行から新しいイメージを使います。実行中のJobは中断せず、通知時に
追加のJobを起動しません。CSVの取得要素を変更する場合も、`main` へのpushで反映します。
ConfigMapやスケジュールなどYAMLの変更は、別途マニフェストを適用してください。

手動実行する場合（実行時刻によって対象サイクルが変わり、RustFSのデータを更新します）:

```sh
kubectl create job --from=cronjob/noaa-gfs-ingest noaa-gfs-ingest-manual -n default
kubectl logs -n default job/noaa-gfs-ingest-manual -f
```

## テスト

Rust単体テストにはecCodesとBloscの開発ライブラリが必要です。コンテナによるテストなら
ホストへ追加インストールせずに実行できます。

```sh
docker build --target builder -t noaa-gfs-ingest:builder .
docker run --rm noaa-gfs-ingest:builder cargo test --locked
```
