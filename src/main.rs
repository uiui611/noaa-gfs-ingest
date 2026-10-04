use anyhow::{Context, Result, bail, ensure};
use aws_config::{BehaviorVersion, Region};
use aws_sdk_s3::error::ProvideErrorMetadata;
use aws_sdk_s3::{Client as S3Client, primitives::ByteStream};
use chrono::{DateTime, Utc};
use noaa_gfs_ingest::{
    IndexEntry, Settings, ZarrWriter, collect_files, completion_marker, load_selections,
    parse_index, select_cycle, source_url, zarr_object_prefix,
};
use reqwest::{Client as HttpClient, StatusCode, header};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

const USER_AGENT: &str = "noaa-gfs-zarr-ingest/4.0";

async fn request_bytes(
    client: &HttpClient,
    url: &str,
    retries: u32,
    range: Option<(u64, u64)>,
) -> Result<Vec<u8>> {
    for attempt in 1..=retries {
        let mut request = client.get(url).header(header::USER_AGENT, USER_AGENT);
        if let Some((start, end)) = range {
            request = request.header(header::RANGE, format!("bytes={start}-{end}"));
        }
        let result = async {
            let response = request.send().await?.error_for_status()?;
            if range.is_some() {
                ensure!(
                    response.status() == StatusCode::PARTIAL_CONTENT,
                    "server ignored Range request (HTTP {}); refusing full download",
                    response.status()
                );
            }
            Ok::<_, anyhow::Error>(response.bytes().await?.to_vec())
        }
        .await;
        match result {
            Ok(bytes) => return Ok(bytes),
            Err(error) if attempt < retries => {
                let delay = 2_u64.pow(attempt).min(30);
                eprintln!("request {attempt}/{retries} failed ({error:#}); retrying in {delay}s");
                tokio::time::sleep(Duration::from_secs(delay)).await;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("request failed after {retries} attempts: {url}"));
            }
        }
    }
    unreachable!()
}

async fn content_length(client: &HttpClient, url: &str, retries: u32) -> Result<u64> {
    for attempt in 1..=retries {
        let result = async {
            let response = client
                .head(url)
                .header(header::USER_AGENT, USER_AGENT)
                .send()
                .await?
                .error_for_status()?;
            Ok::<_, anyhow::Error>(
                response
                    .headers()
                    .get(header::CONTENT_LENGTH)
                    .context("HEAD response has no Content-Length")?
                    .to_str()?
                    .parse()?,
            )
        }
        .await;
        match result {
            Ok(size) => return Ok(size),
            Err(error) if attempt < retries => {
                let delay = 2_u64.pow(attempt).min(30);
                eprintln!("HEAD {attempt}/{retries} failed ({error:#}); retrying in {delay}s");
                tokio::time::sleep(Duration::from_secs(delay)).await;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("HEAD failed after {retries} attempts: {url}"));
            }
        }
    }
    unreachable!()
}

async fn s3_client(settings: &Settings) -> S3Client {
    let mut loader = aws_config::defaults(BehaviorVersion::latest())
        .region(Region::new(settings.s3_region.clone()));
    if let Some(endpoint) = &settings.s3_endpoint_url {
        loader = loader.endpoint_url(endpoint);
    }
    let shared = loader.load().await;
    let config = aws_sdk_s3::config::Builder::from(&shared)
        .force_path_style(true)
        .build();
    S3Client::from_conf(config)
}

async fn object_exists(s3: &S3Client, bucket: &str, key: &str) -> Result<bool> {
    match s3.head_object().bucket(bucket).key(key).send().await {
        Ok(_) => Ok(true),
        Err(error) => {
            let code = error.as_service_error().and_then(|e| e.code());
            if matches!(code, Some("404" | "NoSuchKey" | "NotFound")) {
                Ok(false)
            } else {
                Err(error.into())
            }
        }
    }
}

fn content_type(path: &Path) -> &'static str {
    match path.file_name().and_then(|v| v.to_str()) {
        Some(".zarray" | ".zattrs" | ".zgroup" | ".zmetadata" | "_SUCCESS") => "application/json",
        _ => "application/octet-stream",
    }
}

async fn upload_zarr(
    s3: &S3Client,
    bucket: &str,
    prefix: &str,
    directory: &Path,
    cycle: DateTime<Utc>,
) -> Result<()> {
    let files = collect_files(directory)?;
    for (position, path) in files.iter().enumerate() {
        let relative = path
            .strip_prefix(directory)?
            .to_string_lossy()
            .replace('\\', "/");
        s3.put_object()
            .bucket(bucket)
            .key(format!("{prefix}/{relative}"))
            .content_type(content_type(path))
            .body(ByteStream::from_path(path).await?)
            .send()
            .await?;
        if (position + 1) % 100 == 0 {
            eprintln!("uploaded {}/{} Zarr objects", position + 1, files.len());
        }
    }
    s3.put_object()
        .bucket(bucket)
        .key(format!("{prefix}/_SUCCESS"))
        .content_type("application/json")
        .body(ByteStream::from(completion_marker(cycle, files.len())?))
        .send()
        .await?;
    eprintln!(
        "uploaded {} objects and completion marker to s3://{bucket}/{prefix}",
        files.len()
    );
    Ok(())
}

async fn ingest_raw(
    settings: &Settings,
    cycle: DateTime<Utc>,
    selections: &[noaa_gfs_ingest::Selection],
    s3: &S3Client,
) -> Result<()> {
    let selection_by_key: HashMap<_, _> =
        selections.iter().map(|item| (item.key(), item)).collect();
    let prefix = zarr_object_prefix(&settings, cycle);
    let marker_key = format!("{prefix}/_SUCCESS");
    let marker_exists = object_exists(s3, &settings.s3_bucket, &marker_key).await?;
    if !settings.overwrite && marker_exists {
        eprintln!(
            "completed Zarr already exists, skipping: s3://{}/{prefix}",
            settings.s3_bucket
        );
        return Ok(());
    }
    if settings.overwrite && marker_exists {
        s3.delete_object()
            .bucket(&settings.s3_bucket)
            .key(&marker_key)
            .send()
            .await?;
    }
    eprintln!(
        "selected cycle={} forecast_hours={:?} fields={}",
        cycle.format("%Y-%m-%dT%H:00Z"),
        settings.forecast_hours,
        selections.len()
    );
    let http = HttpClient::builder()
        .timeout(Duration::from_secs(60))
        .build()?;
    let mut found = HashSet::new();
    let mut transferred = 0_u64;
    let temporary = tempfile::Builder::new()
        .prefix("gfs-zarr-")
        .tempdir_in("/tmp")?;
    let mut writer = ZarrWriter::new(
        temporary.path().join("output.zarr"),
        cycle,
        &settings.forecast_hours,
    )?;
    for (forecast_index, &forecast_hour) in settings.forecast_hours.iter().enumerate() {
        let url = source_url(&settings, cycle, forecast_hour);
        let index_text = String::from_utf8(
            request_bytes(
                &http,
                &format!("{url}.idx"),
                settings.download_retries,
                None,
            )
            .await?,
        )?;
        let entries = parse_index(&index_text)?;
        let mut selected: Vec<IndexEntry> = entries
            .into_iter()
            .filter(|entry| selection_by_key.contains_key(&entry.key()))
            .collect();
        if selected.last().is_some_and(|entry| entry.end.is_none()) {
            let size = content_length(&http, &url, settings.download_retries).await?;
            selected.last_mut().unwrap().end = Some(size - 1);
        }
        let matched: HashSet<_> = selected.iter().map(IndexEntry::key).collect();
        for selection in selections {
            if !matched.contains(&selection.key()) {
                eprintln!(
                    "warning: f{forecast_hour:03} does not contain {}",
                    selection.zarr_name
                );
            }
        }
        for entry in selected {
            let end = entry
                .end
                .context("last selected record has no end offset")?;
            let selection = selection_by_key.get(&entry.key()).unwrap();
            eprintln!(
                "f{forecast_hour:03} range={}-{} field={}",
                entry.start, end, selection.zarr_name
            );
            let message = request_bytes(
                &http,
                &url,
                settings.download_retries,
                Some((entry.start, end)),
            )
            .await?;
            ensure!(
                message.len() as u64 == end - entry.start + 1,
                "Range response size mismatch: expected {}, got {}",
                end - entry.start + 1,
                message.len()
            );
            transferred += message.len() as u64;
            writer.write_message(selection, forecast_index, &message)?;
            found.insert(selection.key());
        }
    }
    let missing: Vec<_> = selections
        .iter()
        .filter(|item| !found.contains(&item.key()))
        .map(|item| item.zarr_name.as_str())
        .collect();
    if !missing.is_empty() {
        bail!("selected fields were absent from every forecast hour: {missing:?}");
    }
    writer.consolidate_metadata()?;
    eprintln!(
        "downloaded selected GRIB messages: {:.1} MiB",
        transferred as f64 / 1_048_576.0
    );
    upload_zarr(s3, &settings.s3_bucket, &prefix, &writer.path, cycle).await
}

async fn run(settings: Settings) -> Result<()> {
    let cycle = select_cycle(Utc::now(), settings.source_age_hours, &settings.cycle)?;
    let selections = load_selections(&settings.selection_file)?;
    let s3 = s3_client(&settings).await;
    noaa_gfs_ingest::retention::prune_expired_zarr(&s3, &settings, cycle).await?;
    ingest_raw(&settings, cycle, &selections, &s3).await?;
    noaa_gfs_ingest::forecast::build_stitched_regional_forecast(&s3, &settings, cycle, &selections)
        .await
}

#[tokio::main]
async fn main() {
    if let Err(error) = async { run(Settings::from_env()?).await }.await {
        eprintln!("ingestion failed: {error:#}");
        std::process::exit(1);
    }
}
