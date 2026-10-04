use crate::Settings;
use anyhow::{Result, bail};
use aws_sdk_s3::Client as S3Client;
use aws_sdk_s3::types::{Delete, ObjectIdentifier};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use std::collections::{BTreeSet, HashSet};

const LEGACY_PREFIXES: [&str; 2] = ["noaa/gfs", "forecast/gfs"];

fn parse_cycle(value: &str) -> Option<DateTime<Utc>> {
    if value.len() != 10 {
        return None;
    }
    let date = NaiveDate::parse_from_str(&value[..8], "%Y%m%d").ok()?;
    let hour = value[8..].parse().ok()?;
    date.and_hms_opt(hour, 0, 0).map(|value| value.and_utc())
}

fn object_cycle(root: &str, key: &str) -> Option<DateTime<Utc>> {
    let relative = if root.is_empty() {
        key
    } else {
        key.strip_prefix(&format!("{root}/"))?
    };
    let mut parts = relative.split('/');
    let first = parts.next()?;
    if let Some(timestamp) = first.strip_suffix(".zarr") {
        if timestamp.len() == 10 {
            return parse_cycle(timestamp);
        }
    }
    let date = first.strip_prefix("gfs.")?;
    let hour = parts.next()?;
    if date.len() == 8 && hour.len() == 2 {
        parse_cycle(&format!("{date}{hour}"))
    } else {
        None
    }
}

async fn expired_keys(
    s3: &S3Client,
    bucket: &str,
    root: &str,
    cutoff: DateTime<Utc>,
) -> Result<Vec<String>> {
    let list_prefix = if root.is_empty() {
        String::new()
    } else {
        format!("{root}/")
    };
    let mut continuation_token = None;
    let mut keys = Vec::new();
    loop {
        let mut request = s3.list_objects_v2().bucket(bucket).prefix(&list_prefix);
        if let Some(token) = continuation_token {
            request = request.continuation_token(token);
        }
        let response = request.send().await?;
        for object in response.contents() {
            let Some(key) = object.key() else {
                continue;
            };
            if object_cycle(root, key).is_some_and(|cycle| cycle <= cutoff) {
                keys.push(key.to_owned());
            }
        }
        if !response.is_truncated().unwrap_or(false) {
            break;
        }
        continuation_token = response.next_continuation_token().map(str::to_owned);
    }
    Ok(keys)
}

pub(crate) async fn delete_keys(s3: &S3Client, bucket: &str, keys: &[String]) -> Result<()> {
    for chunk in keys.chunks(1000) {
        let objects = chunk
            .iter()
            .map(|key| ObjectIdentifier::builder().key(key).build())
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let delete = Delete::builder()
            .set_objects(Some(objects))
            .quiet(true)
            .build()?;
        let response = s3
            .delete_objects()
            .bucket(bucket)
            .delete(delete)
            .send()
            .await?;
        if !response.errors().is_empty() {
            let summary = response
                .errors()
                .iter()
                .map(|error| {
                    format!(
                        "{}: {}",
                        error.key().unwrap_or("<unknown>"),
                        error.message().unwrap_or("unknown delete error")
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            bail!("S3 object deletion failed: {summary}");
        }
    }
    Ok(())
}

pub async fn prune_expired_zarr(
    s3: &S3Client,
    settings: &Settings,
    current_cycle: DateTime<Utc>,
) -> Result<()> {
    let cutoff = current_cycle - Duration::days(settings.retention_days);
    let mut roots = BTreeSet::from([
        settings.s3_prefix.as_str(),
        settings.forecast_prefix.as_str(),
    ]);
    roots.extend(LEGACY_PREFIXES);
    let mut keys = HashSet::new();
    for root in roots {
        keys.extend(
            expired_keys(s3, &settings.s3_bucket, root, cutoff)
                .await?
                .into_iter(),
        );
    }
    let mut keys: Vec<_> = keys.into_iter().collect();
    keys.sort();
    delete_keys(s3, &settings.s3_bucket, &keys).await?;
    eprintln!(
        "retention cleanup: deleted {} objects at or before {} ({} days)",
        keys.len(),
        cutoff.format("%Y-%m-%dT%H:00Z"),
        settings.retention_days
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_current_and_legacy_dataset_paths() {
        let expected = "2026-09-05T06:00:00Z".parse().unwrap();
        assert_eq!(
            object_cycle("noaa-gfs", "noaa-gfs/2026090506.zarr/_SUCCESS"),
            Some(expected)
        );
        assert_eq!(
            object_cycle(
                "noaa/gfs",
                "noaa/gfs/gfs.20260905/06/gfs.t06z.pgrb2.0p25.zarr/.zgroup"
            ),
            Some(expected)
        );
        assert_eq!(object_cycle("noaa-gfs", "noaa-gfs/unmanaged/object"), None);
    }

    #[test]
    fn cutoff_includes_exactly_seven_days_ago() {
        let current: DateTime<Utc> = "2026-09-12T06:00:00Z".parse().unwrap();
        let cutoff = current - Duration::days(7);
        assert!(
            object_cycle("forecast", "forecast/2026090506.zarr/.zgroup")
                .is_some_and(|cycle| cycle <= cutoff)
        );
        assert!(
            object_cycle("forecast", "forecast/2026090512.zarr/.zgroup")
                .is_some_and(|cycle| cycle > cutoff)
        );
    }
}
