use anyhow::{Context, Result, ensure};
use noaa_gfs_ingest::statistic;
use regex::Regex;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

fn common_names() -> HashMap<&'static str, &'static str> {
    HashMap::from([
        ("4LFTX", "4層リフティド指数"),
        ("ABSV", "絶対渦度"),
        ("ACPCP", "対流性降水量"),
        ("ALBDO", "アルベド"),
        ("APCP", "総降水量"),
        ("APTMP", "体感温度"),
        ("CAPE", "対流有効位置エネルギー"),
        ("CFRZR", "着氷性降水確率"),
        ("CICEP", "凍雨確率"),
        ("CIN", "対流抑制"),
        ("CLWMR", "雲水混合比"),
        ("CNWAT", "植生冠水量"),
        ("CPOFP", "凍結降水率"),
        ("CPRAT", "対流性降水率"),
        ("CRAIN", "降雨確率"),
        ("CSNOW", "降雪確率"),
        ("CWAT", "鉛直積算雲水量"),
        ("CWORK", "雲仕事関数"),
        ("DLWRF", "下向き長波放射"),
        ("DPT", "露点温度"),
        ("DSWRF", "下向き短波放射"),
        ("DZDT", "鉛直速度（幾何）"),
        ("FLDCP", "圃場容水量"),
        ("FRICV", "摩擦速度"),
        ("GFLUX", "地中熱フラックス"),
        ("GRLE", "霰混合比"),
        ("GUST", "突風速度"),
        ("HCDC", "上層雲量"),
        ("HGT", "ジオポテンシャル高度"),
        ("HINDEX", "ヘインズ指数"),
        ("HLCY", "ストーム相対ヘリシティ"),
        ("HPBL", "境界層高度"),
        ("ICAHT", "着氷高度"),
        ("ICEC", "海氷密接度"),
        ("ICEG", "着氷強度"),
        ("ICETK", "海氷厚"),
        ("ICETMP", "海氷温度"),
        ("ICMR", "雲氷混合比"),
        ("LAND", "陸海マスク"),
        ("LCDC", "下層雲量"),
        ("LFTX", "リフティド指数"),
        ("LHTFL", "潜熱フラックス"),
        ("MCDC", "中層雲量"),
        ("MSLET", "ETA換算海面更正気圧"),
        ("O3MR", "オゾン混合比"),
        ("PEVPR", "潜在蒸発率"),
        ("PLPL", "持ち上げ凝結面気圧"),
        ("POT", "温位"),
        ("PRATE", "降水率"),
        ("PRES", "気圧"),
        ("PRMSL", "海面更正気圧"),
        ("PWAT", "可降水量"),
        ("REFC", "合成レーダー反射強度"),
        ("REFD", "レーダー反射強度"),
        ("RH", "相対湿度"),
        ("RWMR", "雨水混合比"),
        ("SFCR", "表面粗度"),
        ("SHTFL", "顕熱フラックス"),
        ("SNMR", "雪混合比"),
        ("SNOD", "積雪深"),
        ("SOILL", "液体土壌水分"),
        ("SOILW", "土壌水分"),
        ("SOTYP", "土壌型"),
        ("SPFH", "比湿"),
        ("SUNSD", "日照時間"),
        ("TCDC", "全雲量"),
        ("TMAX", "最高気温"),
        ("TMIN", "最低気温"),
        ("TMP", "気温"),
        ("TOZNE", "全オゾン量"),
        ("TSOIL", "土壌温度"),
        ("U-GWD", "重力波抗力東西成分"),
        ("UFLX", "運動量フラックス東西成分"),
        ("UGRD", "東西風"),
        ("ULWRF", "上向き長波放射"),
        ("USWRF", "上向き短波放射"),
        ("USTM", "ストーム移動東西成分"),
        ("VEG", "植生率"),
        ("V-GWD", "重力波抗力南北成分"),
        ("VFLX", "運動量フラックス南北成分"),
        ("VGRD", "南北風"),
        ("VIS", "視程"),
        ("VRATE", "換気率"),
        ("VSTM", "ストーム移動南北成分"),
        ("VVEL", "鉛直速度（気圧）"),
        ("VWSH", "鉛直風シア"),
        ("WATR", "水収支"),
        ("WEASD", "積雪水量"),
        ("WILT", "しおれ点"),
    ])
}

fn enabled(
    short: &str,
    level: &str,
    stat: &str,
    occurrence: u32,
) -> Option<(&'static str, &'static str)> {
    match (short, level, stat, occurrence) {
        ("PRMSL", "mean sea level", "instant", 1) => {
            Some(("mean_sea_level_pressure", "海面更正気圧"))
        }
        ("TMP", "2 m above ground", "instant", 1) => Some(("air_temperature_2m", "地上2m気温")),
        ("RH", "2 m above ground", "instant", 1) => {
            Some(("relative_humidity_2m", "地上2m相対湿度"))
        }
        ("UGRD", "10 m above ground", "instant", 1) => Some(("eastward_wind_10m", "地上10m東西風")),
        ("VGRD", "10 m above ground", "instant", 1) => {
            Some(("northward_wind_10m", "地上10m南北風"))
        }
        ("APCP", "surface", "accumulation", 1) => Some(("precipitation_amount", "総降水量")),
        ("TCDC", "entire atmosphere", "instant", 1) => Some(("cloud_area_fraction", "全雲量")),
        _ => None,
    }
}

fn slug(value: &str) -> String {
    let re = Regex::new(r"[^a-z0-9]+").unwrap();
    let result = re
        .replace_all(&value.to_ascii_lowercase(), "_")
        .trim_matches('_')
        .to_owned();
    if result.is_empty() {
        "field".to_owned()
    } else {
        result
    }
}

fn run(index: &Path, source: &str) -> Result<()> {
    let text = fs::read_to_string(index).with_context(|| format!("read {}", index.display()))?;
    let names = common_names();
    let mut occurrences: HashMap<(String, String, String), u32> = HashMap::new();
    let stdout = io::stdout();
    let mut output = stdout.lock();
    writeln!(output, "## NOAA GFS 0.25 degree pgrb2 selection catalog.")?;
    writeln!(output, "## Source inventory: {source}")?;
    writeln!(
        output,
        "## Lines beginning with one # are disabled; remove/add # to select fields."
    )?;
    writeln!(
        output,
        "short_name,level,statistic,occurrence,zarr_name,common_name_ja"
    )?;
    for line in text.lines() {
        let parts: Vec<_> = line.split(':').collect();
        ensure!(parts.len() >= 7, "invalid idx line: {line:?}");
        let stat = statistic(parts[5]);
        let occurrence = occurrences
            .entry((parts[3].into(), parts[4].into(), stat.into()))
            .or_default();
        *occurrence += 1;
        let selected = enabled(parts[3], parts[4], stat, *occurrence);
        let (zarr_name, common_name) =
            selected.map(|(a, b)| (a.to_owned(), b)).unwrap_or_else(|| {
                (
                    slug(&format!(
                        "{}_{}_{}_{}",
                        parts[3], parts[4], stat, *occurrence
                    )),
                    names.get(parts[3]).copied().unwrap_or(parts[3]),
                )
            });
        if selected.is_none() {
            write!(output, "#")?;
        }
        let mut writer = csv::WriterBuilder::new()
            .has_headers(false)
            .terminator(csv::Terminator::Any(b'\n'))
            .from_writer(vec![]);
        writer.write_record([
            parts[3],
            parts[4],
            stat,
            &occurrence.to_string(),
            &zarr_name,
            common_name,
        ])?;
        output.write_all(&writer.into_inner()?)?;
    }
    Ok(())
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 2 && args.len() != 3 {
        eprintln!("usage: {} INDEX_FILE [SOURCE_URL]", args[0]);
        std::process::exit(2);
    }
    let source = args
        .get(2)
        .map(String::as_str)
        .unwrap_or_else(|| Path::new(&args[1]).file_name().unwrap().to_str().unwrap());
    if let Err(error) = run(Path::new(&args[1]), source) {
        eprintln!("catalog generation failed: {error:#}");
        std::process::exit(1);
    }
}
