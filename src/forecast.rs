use crate::mesh3::Mesh3Grid;
use crate::{
    RegionalZarrWriter, Selection, Settings, blosc_decompress, collect_files,
    forecast_object_prefix, zarr_object_prefix,
};
use anyhow::{Context, Result, ensure};
use aws_sdk_s3::Client as S3Client;
use aws_sdk_s3::primitives::ByteStream;
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Deserialize)]
struct ConsolidatedMetadata {
    metadata: HashMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct ZArrayMetadata {
    shape: Vec<usize>,
    chunks: Vec<usize>,
    dtype: String,
}

async fn object_exists(s3: &S3Client, bucket: &str, key: &str) -> Result<bool> {
    match s3.head_object().bucket(bucket).key(key).send().await {
        Ok(_) => Ok(true),
        Err(error) => {
            use aws_sdk_s3::error::ProvideErrorMetadata;
            let code = error.as_service_error().and_then(|value| value.code());
            if matches!(code, Some("404" | "NoSuchKey" | "NotFound")) {
                Ok(false)
            } else {
                Err(error.into())
            }
        }
    }
}

async fn get_required(s3: &S3Client, bucket: &str, key: &str) -> Result<Vec<u8>> {
    let response = s3
        .get_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .with_context(|| format!("download s3://{bucket}/{key}"))?;
    Ok(response.body.collect().await?.into_bytes().to_vec())
}

async fn get_optional(s3: &S3Client, bucket: &str, key: &str) -> Result<Option<Vec<u8>>> {
    if !object_exists(s3, bucket, key).await? {
        return Ok(None);
    }
    Ok(Some(get_required(s3, bucket, key).await?))
}

fn decode_f32(bytes: &[u8]) -> Result<Vec<f32>> {
    ensure!(
        bytes.len() % 4 == 0,
        "f32 Zarr chunk byte length is invalid"
    );
    Ok(bytes
        .chunks_exact(4)
        .map(|value| f32::from_le_bytes(value.try_into().unwrap()))
        .collect())
}

fn decode_i32(bytes: &[u8]) -> Result<Vec<i32>> {
    ensure!(
        bytes.len() % 4 == 0,
        "i32 Zarr chunk byte length is invalid"
    );
    Ok(bytes
        .chunks_exact(4)
        .map(|value| i32::from_le_bytes(value.try_into().unwrap()))
        .collect())
}

fn array_metadata(metadata: &ConsolidatedMetadata, name: &str) -> Result<ZArrayMetadata> {
    let key = format!("{name}/.zarray");
    serde_json::from_value(
        metadata
            .metadata
            .get(&key)
            .with_context(|| format!("Zarr metadata is missing {key}"))?
            .clone(),
    )
    .map_err(Into::into)
}

fn array_attrs<'a>(metadata: &'a ConsolidatedMetadata, name: &str) -> Result<&'a Value> {
    let key = format!("{name}/.zattrs");
    metadata
        .metadata
        .get(&key)
        .with_context(|| format!("Zarr metadata is missing {key}"))
}

// Include every setting that changes the output so old 0.25-degree outputs
// and outputs for different bounds/fields are not mistaken for mesh3 data.
fn processing_descriptor(settings: &Settings, selections: &[Selection]) -> Value {
    json!({
        "version":1,
        "grid":"japan_mesh3",
        "registration":"cell_center",
        "interpolation":"bilinear",
        "bounds":[settings.forecast_lat_min, settings.forecast_lat_max,
                  settings.forecast_lon_min, settings.forecast_lon_max],
        "fields":selections.iter().map(|s| &s.zarr_name).collect::<Vec<_>>()
    })
}

fn completed_for_processing(marker: &[u8], processing: &Value) -> bool {
    serde_json::from_slice::<Value>(marker)
        .ok()
        .is_some_and(|value| value.get("processing") == Some(processing))
}

async fn read_region_slice(
    s3: &S3Client,
    bucket: &str,
    prefix: &str,
    name: &str,
    source_time: usize,
    metadata: &ZArrayMetadata,
    j_range: (usize, usize),
    i_range: (usize, usize),
) -> Result<Vec<f32>> {
    ensure!(
        metadata.dtype == "<f4",
        "unsupported dtype for {name}: {}",
        metadata.dtype
    );
    ensure!(
        metadata.shape.len() == 3 && metadata.chunks.len() == 3,
        "{name} is not a 3-D Zarr array"
    );
    ensure!(metadata.chunks[0] == 1, "{name} time chunk must be 1");
    ensure!(
        source_time < metadata.shape[0],
        "source time index is out of range for {name}"
    );
    let (j_start, j_end) = j_range;
    let (i_start, i_end) = i_range;
    ensure!(
        j_end < metadata.shape[1] && i_end < metadata.shape[2],
        "regional bounds exceed {name} shape"
    );
    let output_ni = i_end - i_start + 1;
    let mut output = vec![f32::NAN; (j_end - j_start + 1) * output_ni];
    let chunk_j = metadata.chunks[1];
    let chunk_i = metadata.chunks[2];
    ensure!(
        chunk_j > 0 && chunk_i > 0,
        "{name} has an empty chunk dimension"
    );
    for cj in (j_start / chunk_j)..=(j_end / chunk_j) {
        for ci in (i_start / chunk_i)..=(i_end / chunk_i) {
            let key = format!("{prefix}/{name}/{source_time}.{cj}.{ci}");
            let Some(compressed) = get_optional(s3, bucket, &key).await? else {
                continue;
            };
            let raw = blosc_decompress(&compressed, chunk_j * chunk_i * 4)?;
            let values = decode_f32(&raw)?;
            let source_j_start = (cj * chunk_j).max(j_start);
            let source_j_end = ((cj + 1) * chunk_j - 1).min(j_end);
            let source_i_start = (ci * chunk_i).max(i_start);
            let source_i_end = ((ci + 1) * chunk_i - 1).min(i_end);
            for source_j in source_j_start..=source_j_end {
                let source_offset =
                    (source_j - cj * chunk_j) * chunk_i + (source_i_start - ci * chunk_i);
                let output_offset = (source_j - j_start) * output_ni + (source_i_start - i_start);
                let count = source_i_end - source_i_start + 1;
                output[output_offset..output_offset + count]
                    .copy_from_slice(&values[source_offset..source_offset + count]);
            }
        }
    }
    Ok(output)
}

fn content_type(path: &Path) -> &'static str {
    match path.file_name().and_then(|value| value.to_str()) {
        Some(".zarray" | ".zattrs" | ".zgroup" | ".zmetadata" | "_SUCCESS") => "application/json",
        _ => "application/octet-stream",
    }
}

async fn upload_zarr(
    s3: &S3Client,
    bucket: &str,
    prefix: &str,
    directory: &Path,
    reference_cycle: DateTime<Utc>,
    uploaded_chunks: usize,
    processing: &Value,
) -> Result<()> {
    let files = collect_files(directory)?;
    for path in &files {
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
    }
    s3.put_object()
        .bucket(bucket)
        .key(format!("{prefix}/_SUCCESS"))
        .content_type("application/json")
        .body(ByteStream::from(serde_json::to_vec(&json!({
            "cycle":reference_cycle.to_rfc3339(),
            "objects":files.len() + uploaded_chunks,
            "processing":processing
        }))?))
        .send()
        .await?;
    eprintln!(
        "uploaded stitched regional forecast: {} objects to s3://{bucket}/{prefix}",
        files.len() + uploaded_chunks
    );
    Ok(())
}

// Retain metadata locally for consolidation, but upload and discard each
// completed time slice before producing the next one (bounded /tmp usage).
async fn upload_slice(
    s3: &S3Client,
    bucket: &str,
    prefix: &str,
    root: &Path,
    chunks: &[std::path::PathBuf],
) -> Result<()> {
    for path in chunks {
        let relative = path
            .strip_prefix(root)?
            .to_string_lossy()
            .replace('\\', "/");
        s3.put_object()
            .bucket(bucket)
            .key(format!("{prefix}/{relative}"))
            .body(ByteStream::from_path(path).await?)
            .send()
            .await?;
        std::fs::remove_file(path)?;
    }
    Ok(())
}

// Replace exactly this derived dataset, including arrays removed from the CSV
// and partial chunks left by a failed attempt. Raw/source prefixes are guarded
// at the call site; the trailing slash prevents matching another dataset.
async fn clear_output(s3: &S3Client, bucket: &str, prefix: &str) -> Result<()> {
    let prefix = format!("{prefix}/");
    let mut token = None;
    let mut keys = Vec::new();
    loop {
        let response = s3
            .list_objects_v2()
            .bucket(bucket)
            .prefix(&prefix)
            .set_continuation_token(token)
            .send()
            .await?;
        for object in response.contents() {
            if let Some(key) = object.key() {
                ensure!(
                    key.starts_with(&prefix),
                    "S3 returned an object outside the output dataset"
                );
                keys.push(key.to_owned());
            }
        }
        if !response.is_truncated().unwrap_or(false) {
            break;
        }
        token = Some(
            response
                .next_continuation_token()
                .context("truncated S3 listing has no continuation token")?
                .to_owned(),
        );
    }
    crate::retention::delete_keys(s3, bucket, &keys).await
}

pub async fn build_stitched_regional_forecast(
    s3: &S3Client,
    settings: &Settings,
    latest_cycle: DateTime<Utc>,
    selections: &[Selection],
) -> Result<()> {
    if !settings.forecast_enabled {
        eprintln!("stitched regional forecast is disabled");
        return Ok(());
    }
    let expected_hours: Vec<u32> = (0..=24).step_by(3).collect();
    ensure!(
        settings.forecast_hours == expected_hours,
        "stitched 48-hour forecast requires GFS_FORECAST_HOURS=0-24:3"
    );
    let previous_cycle = latest_cycle - Duration::hours(24);
    let previous_prefix = zarr_object_prefix(settings, previous_cycle);
    let latest_prefix = zarr_object_prefix(settings, latest_cycle);
    for prefix in [&previous_prefix, &latest_prefix] {
        if !object_exists(s3, &settings.s3_bucket, &format!("{prefix}/_SUCCESS")).await? {
            eprintln!(
                "source NOAA Zarr is absent; stitched regional forecast skipped: s3://{}/{prefix}",
                settings.s3_bucket
            );
            return Ok(());
        }
    }
    let output_prefix = forecast_object_prefix(settings, previous_cycle);
    ensure!(
        output_prefix != previous_prefix && output_prefix != latest_prefix,
        "regional output prefix must differ from the source datasets"
    );
    let marker_key = format!("{output_prefix}/_SUCCESS");
    let processing = processing_descriptor(settings, selections);
    let marker = get_optional(s3, &settings.s3_bucket, &marker_key).await?;
    if marker
        .as_deref()
        .is_some_and(|bytes| completed_for_processing(bytes, &processing))
        && !settings.overwrite
    {
        eprintln!(
            "stitched regional forecast already exists, skipping: s3://{}/{output_prefix}",
            settings.s3_bucket
        );
        return Ok(());
    }
    let previous_metadata: ConsolidatedMetadata = serde_json::from_slice(
        &get_required(
            s3,
            &settings.s3_bucket,
            &format!("{previous_prefix}/.zmetadata"),
        )
        .await?,
    )?;
    let latest_metadata: ConsolidatedMetadata = serde_json::from_slice(
        &get_required(
            s3,
            &settings.s3_bucket,
            &format!("{latest_prefix}/.zmetadata"),
        )
        .await?,
    )?;
    let latitudes = decode_f32(
        &get_required(
            s3,
            &settings.s3_bucket,
            &format!("{previous_prefix}/latitude/0"),
        )
        .await?,
    )?;
    let longitudes = decode_f32(
        &get_required(
            s3,
            &settings.s3_bucket,
            &format!("{previous_prefix}/longitude/0"),
        )
        .await?,
    )?;
    for (name, expected) in [("latitude", &latitudes), ("longitude", &longitudes)] {
        let latest = decode_f32(
            &get_required(
                s3,
                &settings.s3_bucket,
                &format!("{latest_prefix}/{name}/0"),
            )
            .await?,
        )?;
        ensure!(
            &latest == expected,
            "source NOAA Zarr {name} coordinates differ"
        );
    }
    let grid = Mesh3Grid::new(
        &latitudes,
        &longitudes,
        settings.forecast_lat_min,
        settings.forecast_lat_max,
        settings.forecast_lon_min,
        settings.forecast_lon_max,
    )?;
    let previous_hours = decode_i32(
        &get_required(
            s3,
            &settings.s3_bucket,
            &format!("{previous_prefix}/forecast_hour/0"),
        )
        .await?,
    )?;
    let latest_hours = decode_i32(
        &get_required(
            s3,
            &settings.s3_bucket,
            &format!("{latest_prefix}/forecast_hour/0"),
        )
        .await?,
    )?;
    ensure!(
        previous_hours
            == expected_hours
                .iter()
                .map(|&value| value as i32)
                .collect::<Vec<_>>()
            && latest_hours == previous_hours,
        "source NOAA Zarr forecast-hour coordinates are incompatible"
    );
    let output_hours: Vec<u32> = (0..=48).step_by(3).collect();
    // Validate every field before invalidating an existing completed output.
    for selection in selections {
        let previous = array_metadata(&previous_metadata, &selection.zarr_name)?;
        let latest = array_metadata(&latest_metadata, &selection.zarr_name)?;
        ensure!(
            previous == latest,
            "source Zarr arrays differ for {}",
            selection.zarr_name
        );
        ensure!(
            previous.shape == vec![previous_hours.len(), latitudes.len(), longitudes.len()],
            "source Zarr array shape does not match coordinates for {}",
            selection.zarr_name
        );
        ensure!(
            previous.dtype == "<f4"
                && previous.chunks.len() == 3
                && previous.chunks[0] == 1
                && previous.chunks.iter().all(|&n| n > 0),
            "unsupported source Zarr dtype/chunks for {}",
            selection.zarr_name
        );
        ensure!(
            array_attrs(&previous_metadata, &selection.zarr_name)?.is_object(),
            "source Zarr attributes must be an object for {}",
            selection.zarr_name
        );
    }
    if marker.is_some() {
        s3.delete_object()
            .bucket(&settings.s3_bucket)
            .key(&marker_key)
            .send()
            .await?;
    }
    clear_output(s3, &settings.s3_bucket, &output_prefix).await?;
    let temporary = tempfile::Builder::new()
        .prefix("gfs-forecast-")
        .tempdir_in("/tmp")?;
    let mut writer = RegionalZarrWriter::new(
        temporary.path().join("output.zarr"),
        previous_cycle,
        latest_cycle,
        &output_hours,
        &grid.latitudes,
        &grid.longitudes,
    )?;
    eprintln!(
        "building stitched forecast reference={} latest={} region={}x{} lat={:.2}..{:.2} lon={:.2}..{:.2}",
        previous_cycle.format("%Y-%m-%dT%H:00Z"),
        latest_cycle.format("%Y-%m-%dT%H:00Z"),
        grid.latitudes.len(),
        grid.longitudes.len(),
        grid.latitudes.iter().copied().fold(f64::INFINITY, f64::min),
        grid.latitudes
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max),
        grid.longitudes[0],
        grid.longitudes[grid.longitudes.len() - 1]
    );
    let mut uploaded_chunks = 0;
    for selection in selections {
        let previous_array = array_metadata(&previous_metadata, &selection.zarr_name)?;
        let latest_array = array_metadata(&latest_metadata, &selection.zarr_name)?;
        ensure!(
            previous_array == latest_array,
            "source Zarr arrays differ for {}",
            selection.zarr_name
        );
        let attrs = array_attrs(&previous_metadata, &selection.zarr_name)?;
        let mut output_index = 0;
        for source_index in 0..previous_hours.len() {
            let values = read_region_slice(
                s3,
                &settings.s3_bucket,
                &previous_prefix,
                &selection.zarr_name,
                source_index,
                &previous_array,
                grid.j_range,
                grid.i_range,
            )
            .await?;
            let interpolated = grid.interpolate(&values)?;
            let paths =
                writer.write_slice(&selection.zarr_name, output_index, &interpolated, attrs)?;
            upload_slice(
                s3,
                &settings.s3_bucket,
                &output_prefix,
                &writer.path,
                &paths,
            )
            .await?;
            uploaded_chunks += paths.len();
            output_index += 1;
        }
        for source_index in 1..latest_hours.len() {
            let values = read_region_slice(
                s3,
                &settings.s3_bucket,
                &latest_prefix,
                &selection.zarr_name,
                source_index,
                &latest_array,
                grid.j_range,
                grid.i_range,
            )
            .await?;
            let interpolated = grid.interpolate(&values)?;
            let paths =
                writer.write_slice(&selection.zarr_name, output_index, &interpolated, attrs)?;
            upload_slice(
                s3,
                &settings.s3_bucket,
                &output_prefix,
                &writer.path,
                &paths,
            )
            .await?;
            uploaded_chunks += paths.len();
            output_index += 1;
        }
        ensure!(
            output_index == output_hours.len(),
            "stitched time count mismatch"
        );
    }
    writer.consolidate_metadata()?;
    upload_zarr(
        s3,
        &settings.s3_bucket,
        &output_prefix,
        &writer.path,
        previous_cycle,
        uploaded_chunks,
        &processing,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_requires_matching_processing() {
        let processing = json!({"version":1,"grid":"japan_mesh3","bounds":[22.4,47.6,120,150]});
        assert!(!completed_for_processing(
            br#"{"cycle":"2026-10-01T00:00:00Z","objects":123}"#,
            &processing
        ));
        assert!(!completed_for_processing(b"invalid JSON", &processing));
        let marker = serde_json::to_vec(&json!({"processing":processing})).unwrap();
        assert!(completed_for_processing(&marker, &processing));
        let mut changed = processing.clone();
        changed["bounds"] = json!([30, 40, 130, 140]);
        assert!(!completed_for_processing(&marker, &changed));
    }
}
