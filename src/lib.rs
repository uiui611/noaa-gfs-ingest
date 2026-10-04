use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Duration, Timelike, Utc};
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{CStr, CString, c_char, c_int, c_long, c_void};
use std::fs;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::OnceLock;

pub mod forecast;
pub mod mesh3;
pub mod retention;

pub const VALID_CYCLES: [u32; 4] = [0, 6, 12, 18];

#[derive(Clone, Debug)]
pub struct Settings {
    pub source_base_url: String,
    pub source_age_hours: i64,
    pub cycle: String,
    pub forecast_hours: Vec<u32>,
    pub selection_file: PathBuf,
    pub s3_endpoint_url: Option<String>,
    pub s3_region: String,
    pub s3_bucket: String,
    pub s3_prefix: String,
    pub forecast_enabled: bool,
    pub forecast_prefix: String,
    pub forecast_lat_min: f64,
    pub forecast_lat_max: f64,
    pub forecast_lon_min: f64,
    pub forecast_lon_max: f64,
    pub retention_days: i64,
    pub overwrite: bool,
    pub download_retries: u32,
}

impl Settings {
    pub fn from_env() -> Result<Self> {
        let bucket = env("S3_BUCKET", "").trim().to_owned();
        ensure!(!bucket.is_empty(), "S3_BUCKET is required");
        let cycle = env("GFS_CYCLE", "auto").trim().to_ascii_lowercase();
        ensure!(
            matches!(cycle.as_str(), "auto" | "00" | "06" | "12" | "18"),
            "GFS_CYCLE must be auto, 00, 06, 12, or 18"
        );
        let source_age_hours = env("SOURCE_AGE_HOURS", "24")
            .parse::<i64>()
            .context("SOURCE_AGE_HOURS must be an integer")?;
        ensure!(
            (0..=240).contains(&source_age_hours),
            "SOURCE_AGE_HOURS must be between 0 and 240"
        );
        let download_retries = env("DOWNLOAD_RETRIES", "4")
            .parse::<u32>()
            .context("DOWNLOAD_RETRIES must be an integer")?;
        ensure!(
            (1..=10).contains(&download_retries),
            "DOWNLOAD_RETRIES must be between 1 and 10"
        );
        let retention_days = env("RETENTION_DAYS", "7")
            .parse::<i64>()
            .context("RETENTION_DAYS must be an integer")?;
        ensure!(
            (1..=365).contains(&retention_days),
            "RETENTION_DAYS must be between 1 and 365"
        );
        let forecast_lat_min = env("FORECAST_LAT_MIN", "22.4")
            .parse::<f64>()
            .context("FORECAST_LAT_MIN must be a number")?;
        let forecast_lat_max = env("FORECAST_LAT_MAX", "47.6")
            .parse::<f64>()
            .context("FORECAST_LAT_MAX must be a number")?;
        let forecast_lon_min = env("FORECAST_LON_MIN", "120.0")
            .parse::<f64>()
            .context("FORECAST_LON_MIN must be a number")?;
        let forecast_lon_max = env("FORECAST_LON_MAX", "150.0")
            .parse::<f64>()
            .context("FORECAST_LON_MAX must be a number")?;
        ensure!(
            forecast_lat_min <= forecast_lat_max,
            "FORECAST_LAT_MIN must not exceed FORECAST_LAT_MAX"
        );
        ensure!(
            forecast_lon_min <= forecast_lon_max,
            "FORECAST_LON_MIN must not exceed FORECAST_LON_MAX"
        );
        Ok(Self {
            source_base_url: env(
                "GFS_SOURCE_BASE_URL",
                "https://nomads.ncep.noaa.gov/pub/data/nccf/com/gfs/prod",
            )
            .trim_end_matches('/')
            .to_owned(),
            source_age_hours,
            cycle,
            forecast_hours: parse_forecast_hours(&env("GFS_FORECAST_HOURS", "0-24:3"))?,
            selection_file: PathBuf::from(env("GFS_SELECTION_FILE", "/app/gfs-fields.csv")),
            s3_endpoint_url: std::env::var("S3_ENDPOINT_URL")
                .ok()
                .filter(|v| !v.is_empty()),
            s3_region: env("AWS_REGION", "us-east-1"),
            s3_bucket: bucket,
            s3_prefix: env("S3_PREFIX", "noaa-gfs").trim_matches('/').to_owned(),
            forecast_enabled: parse_bool(&env("FORECAST_ENABLED", "true"))?,
            forecast_prefix: env("FORECAST_PREFIX", "forecast")
                .trim_matches('/')
                .to_owned(),
            forecast_lat_min,
            forecast_lat_max,
            forecast_lon_min,
            forecast_lon_max,
            retention_days,
            overwrite: parse_bool(&env("OVERWRITE", "false"))?,
            download_retries,
        })
    }
}

fn env(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Selection {
    pub short_name: String,
    pub level: String,
    pub statistic: String,
    pub occurrence: u32,
    pub zarr_name: String,
    pub common_name_ja: String,
}

impl Selection {
    pub fn key(&self) -> SelectorKey {
        (
            self.short_name.clone(),
            self.level.clone(),
            self.statistic.clone(),
            self.occurrence,
        )
    }
}

pub type SelectorKey = (String, String, String, u32);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexEntry {
    pub record: u32,
    pub start: u64,
    pub end: Option<u64>,
    pub short_name: String,
    pub level: String,
    pub statistic: String,
    pub occurrence: u32,
    pub forecast_description: String,
}

impl IndexEntry {
    pub fn key(&self) -> SelectorKey {
        (
            self.short_name.clone(),
            self.level.clone(),
            self.statistic.clone(),
            self.occurrence,
        )
    }
}

pub fn parse_bool(value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" => Ok(true),
        "0" | "false" | "no" => Ok(false),
        _ => bail!("invalid boolean value: {value:?}"),
    }
}

pub fn parse_forecast_hours(spec: &str) -> Result<Vec<u32>> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"^(\d{1,3})(?:-(\d{1,3})(?::(\d{1,3}))?)?$").unwrap());
    let mut hours = HashSet::new();
    for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let cap = re
            .captures(part)
            .with_context(|| format!("invalid GFS_FORECAST_HOURS item: {part:?}"))?;
        let start: u32 = cap[1].parse()?;
        let end: u32 = cap.get(2).map_or(Ok(start), |v| v.as_str().parse())?;
        let step: u32 = cap.get(3).map_or(Ok(1), |v| v.as_str().parse())?;
        ensure!(
            start <= end && step >= 1 && end <= 384,
            "invalid GFS forecast-hour range: {part:?}"
        );
        let mut hour = start;
        while hour <= end {
            hours.insert(hour);
            hour = hour
                .checked_add(step)
                .context("forecast-hour range overflow")?;
        }
    }
    ensure!(
        !hours.is_empty(),
        "GFS_FORECAST_HOURS must contain at least one hour"
    );
    let mut result: Vec<_> = hours.into_iter().collect();
    result.sort_unstable();
    Ok(result)
}

pub fn statistic(description: &str) -> &'static str {
    static INSTANT: OnceLock<Regex> = OnceLock::new();
    if description == "anl"
        || INSTANT
            .get_or_init(|| Regex::new(r"^\d+ hour fcst$").unwrap())
            .is_match(description)
    {
        return "instant";
    }
    for (suffix, value) in [
        (" acc fcst", "accumulation"),
        (" ave fcst", "average"),
        (" max fcst", "maximum"),
        (" min fcst", "minimum"),
    ] {
        if description.ends_with(suffix) {
            return value;
        }
    }
    "other"
}

pub fn select_cycle(
    now: DateTime<Utc>,
    age_hours: i64,
    configured_cycle: &str,
) -> Result<DateTime<Utc>> {
    let target = now - Duration::hours(age_hours);
    let hour = if configured_cycle == "auto" {
        *VALID_CYCLES
            .iter()
            .filter(|&&h| h <= target.hour())
            .max()
            .unwrap()
    } else {
        configured_cycle
            .parse::<u32>()
            .context("invalid configured cycle")?
    };
    Ok(target
        .with_hour(hour)
        .unwrap()
        .with_minute(0)
        .unwrap()
        .with_second(0)
        .unwrap()
        .with_nanosecond(0)
        .unwrap())
}

pub fn source_url(settings: &Settings, cycle: DateTime<Utc>, forecast_hour: u32) -> String {
    format!(
        "{}/gfs.{}/{}/atmos/gfs.t{}z.pgrb2.0p25.f{:03}",
        settings.source_base_url,
        cycle.format("%Y%m%d"),
        cycle.format("%H"),
        cycle.format("%H"),
        forecast_hour
    )
}

pub fn zarr_object_prefix(settings: &Settings, cycle: DateTime<Utc>) -> String {
    let relative = format!("{}.zarr", cycle.format("%Y%m%d%H"));
    if settings.s3_prefix.is_empty() {
        relative
    } else {
        format!("{}/{relative}", settings.s3_prefix)
    }
}

pub fn forecast_object_prefix(settings: &Settings, reference_cycle: DateTime<Utc>) -> String {
    let relative = format!("{}.zarr", reference_cycle.format("%Y%m%d%H"));
    if settings.forecast_prefix.is_empty() {
        relative
    } else {
        format!("{}/{relative}", settings.forecast_prefix)
    }
}

pub fn load_selections(path: &Path) -> Result<Vec<Selection>> {
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut header = None;
    let mut active = Vec::new();
    for line in text.lines() {
        if line.starts_with("##") || line.trim().is_empty() {
            continue;
        }
        if header.is_none() {
            header = Some(line.to_owned());
        } else if !line.trim_start().starts_with('#') {
            active.push(line.to_owned());
        }
    }
    let header = header.with_context(|| format!("CSV header is missing: {}", path.display()))?;
    let csv_text = std::iter::once(header)
        .chain(active)
        .collect::<Vec<_>>()
        .join("\n");
    let mut reader = csv::Reader::from_reader(csv_text.as_bytes());
    let selections: Vec<Selection> = reader
        .deserialize()
        .collect::<std::result::Result<_, _>>()?;
    ensure!(!selections.is_empty(), "selection CSV has no enabled rows");
    let mut keys = HashSet::new();
    let mut names = HashSet::new();
    static NAME: OnceLock<Regex> = OnceLock::new();
    let name_re = NAME.get_or_init(|| Regex::new(r"^[a-z][a-z0-9_]*$").unwrap());
    for item in &selections {
        ensure!(
            keys.insert(item.key()),
            "selection CSV contains duplicate selectors"
        );
        ensure!(
            names.insert(item.zarr_name.clone()),
            "selection CSV contains duplicate zarr_name values"
        );
        ensure!(
            name_re.is_match(&item.zarr_name),
            "invalid zarr_name: {:?}",
            item.zarr_name
        );
    }
    Ok(selections)
}

pub fn parse_index(text: &str) -> Result<Vec<IndexEntry>> {
    let mut occurrences: HashMap<(String, String, String), u32> = HashMap::new();
    let mut entries = Vec::new();
    for line in text.lines() {
        let parts: Vec<_> = line.split(':').collect();
        ensure!(parts.len() >= 7, "invalid NOAA idx line: {line:?}");
        let stat = statistic(parts[5]).to_owned();
        let occurrence = occurrences
            .entry((parts[3].to_owned(), parts[4].to_owned(), stat.clone()))
            .or_default();
        *occurrence += 1;
        entries.push(IndexEntry {
            record: parts[0].parse()?,
            start: parts[1].parse()?,
            end: None,
            short_name: parts[3].to_owned(),
            level: parts[4].to_owned(),
            statistic: stat,
            occurrence: *occurrence,
            forecast_description: parts[5].to_owned(),
        });
    }
    for index in 0..entries.len().saturating_sub(1) {
        entries[index].end = Some(entries[index + 1].start - 1);
    }
    Ok(entries)
}

#[link(name = "eccodes")]
unsafe extern "C" {
    fn codes_handle_new_from_message_copy(
        context: *mut c_void,
        data: *const c_void,
        length: usize,
    ) -> *mut c_void;
    fn codes_handle_delete(handle: *mut c_void) -> c_int;
    fn codes_get_long(handle: *const c_void, key: *const c_char, value: *mut c_long) -> c_int;
    fn codes_get_double(handle: *const c_void, key: *const c_char, value: *mut f64) -> c_int;
    fn codes_get_string(
        handle: *const c_void,
        key: *const c_char,
        value: *mut c_char,
        length: *mut usize,
    ) -> c_int;
    fn codes_get_size(handle: *const c_void, key: *const c_char, size: *mut usize) -> c_int;
    fn codes_get_double_array(
        handle: *const c_void,
        key: *const c_char,
        values: *mut f64,
        length: *mut usize,
    ) -> c_int;
    fn codes_get_error_message(code: c_int) -> *const c_char;
}

#[link(name = "blosc")]
unsafe extern "C" {
    fn blosc_compress_ctx(
        clevel: c_int,
        doshuffle: c_int,
        typesize: usize,
        nbytes: usize,
        src: *const c_void,
        dest: *mut c_void,
        destsize: usize,
        compressor: *const c_char,
        blocksize: usize,
        numinternalthreads: c_int,
    ) -> c_int;
    fn blosc_decompress_ctx(
        src: *const c_void,
        dest: *mut c_void,
        destsize: usize,
        numinternalthreads: c_int,
    ) -> c_int;
}

fn codes_result(code: c_int, operation: &str) -> Result<()> {
    if code == 0 {
        return Ok(());
    }
    let message = unsafe { CStr::from_ptr(codes_get_error_message(code)) }.to_string_lossy();
    bail!("ecCodes {operation} failed: {message} ({code})")
}

pub struct GribMessage {
    handle: *mut c_void,
}

impl GribMessage {
    pub fn decode(message: &[u8]) -> Result<Self> {
        ensure!(
            message.starts_with(b"GRIB") && message.ends_with(b"7777"),
            "invalid GRIB message framing"
        );
        let handle = unsafe {
            codes_handle_new_from_message_copy(
                ptr::null_mut(),
                message.as_ptr().cast(),
                message.len(),
            )
        };
        ensure!(!handle.is_null(), "ecCodes could not decode GRIB message");
        Ok(Self { handle })
    }
    pub fn long(&self, key: &str) -> Result<i64> {
        let key = CString::new(key)?;
        let mut value: c_long = 0;
        codes_result(
            unsafe { codes_get_long(self.handle, key.as_ptr(), &mut value) },
            key.to_str()?,
        )?;
        Ok(value as i64)
    }
    pub fn double(&self, key: &str) -> Result<f64> {
        let key = CString::new(key)?;
        let mut value = 0.0;
        codes_result(
            unsafe { codes_get_double(self.handle, key.as_ptr(), &mut value) },
            key.to_str()?,
        )?;
        Ok(value)
    }
    pub fn string(&self, key: &str) -> Result<String> {
        let key = CString::new(key)?;
        // ecCodes codes_get_size() returns the number of values, not the byte
        // length of a string concept. GRIB names and units are bounded and far
        // smaller than this buffer.
        let mut bytes = vec![0_u8; 1024];
        let mut capacity = bytes.len();
        codes_result(
            unsafe {
                codes_get_string(
                    self.handle,
                    key.as_ptr(),
                    bytes.as_mut_ptr().cast(),
                    &mut capacity,
                )
            },
            key.to_str()?,
        )?;
        Ok(CStr::from_bytes_until_nul(&bytes)?
            .to_string_lossy()
            .into_owned())
    }
    pub fn array(&self, key: &str) -> Result<Vec<f64>> {
        let key = CString::new(key)?;
        let mut length = 0;
        codes_result(
            unsafe { codes_get_size(self.handle, key.as_ptr(), &mut length) },
            key.to_str()?,
        )?;
        let mut values = vec![0.0; length];
        let mut actual = length;
        codes_result(
            unsafe {
                codes_get_double_array(self.handle, key.as_ptr(), values.as_mut_ptr(), &mut actual)
            },
            key.to_str()?,
        )?;
        values.truncate(actual);
        Ok(values)
    }
}

impl Drop for GribMessage {
    fn drop(&mut self) {
        unsafe {
            codes_handle_delete(self.handle);
        }
    }
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    fs::write(path, serde_json::to_vec(value)?).with_context(|| format!("write {}", path.display()))
}

fn metadata_array(
    shape: &[usize],
    chunks: &[usize],
    dtype: &str,
    fill_value: Value,
    compressed: bool,
) -> Value {
    json!({"chunks": chunks, "compressor": if compressed { json!({"blocksize":0,"clevel":3,"cname":"zstd","id":"blosc","shuffle":2}) } else { Value::Null },
        "dtype": dtype, "fill_value": fill_value, "filters": null, "order": "C", "shape": shape, "zarr_format": 2})
}

fn as_le_f32(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}
fn as_le_f64(values: &[f64]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}
fn as_le_i32(values: &[i32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}
fn as_le_i64(values: &[i64]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

pub fn blosc_zstd_bitshuffle(input: &[u8], typesize: usize) -> Result<Vec<u8>> {
    let compressor = c"zstd";
    let mut output = vec![0_u8; input.len() + 16];
    let size = unsafe {
        blosc_compress_ctx(
            3,
            2,
            typesize,
            input.len(),
            input.as_ptr().cast(),
            output.as_mut_ptr().cast(),
            output.len(),
            compressor.as_ptr(),
            0,
            1,
        )
    };
    ensure!(size > 0, "Blosc compression failed");
    output.truncate(size as usize);
    Ok(output)
}

pub fn blosc_decompress(input: &[u8], expected_size: usize) -> Result<Vec<u8>> {
    ensure!(input.len() >= 16, "Blosc chunk is shorter than its header");
    let declared_size = u32::from_le_bytes(input[4..8].try_into().unwrap()) as usize;
    ensure!(
        declared_size == expected_size,
        "Blosc uncompressed size mismatch: expected {expected_size}, header declares {declared_size}"
    );
    let mut output = vec![0_u8; expected_size];
    let size = unsafe {
        blosc_decompress_ctx(
            input.as_ptr().cast(),
            output.as_mut_ptr().cast(),
            output.len(),
            1,
        )
    };
    ensure!(size >= 0, "Blosc decompression failed ({size})");
    ensure!(
        size as usize == expected_size,
        "Blosc decompressed size mismatch: expected {expected_size}, got {size}"
    );
    Ok(output)
}

pub struct RegionalZarrWriter {
    pub path: PathBuf,
    forecast_count: usize,
    nj: usize,
    ni: usize,
    arrays: HashSet<String>,
}

impl RegionalZarrWriter {
    pub fn new(
        path: PathBuf,
        reference_cycle: DateTime<Utc>,
        latest_cycle: DateTime<Utc>,
        forecast_hours: &[u32],
        latitudes: &[f64],
        longitudes: &[f64],
    ) -> Result<Self> {
        ensure!(
            !latitudes.is_empty() && !longitudes.is_empty(),
            "regional coordinates are empty"
        );
        fs::create_dir_all(&path)?;
        write_json(&path.join(".zgroup"), &json!({"zarr_format":2}))?;
        write_json(
            &path.join(".zattrs"),
            &json!({
                "Conventions":"CF-1.10",
                "title":"NOAA NCEP GFS stitched 48-hour forecast on Japan third-order regional mesh cell centers",
                "source":"Two consecutive NOAA NOMADS GFS pgrb2.0p25 selected-field Zarr datasets",
                "forecast_reference_time":reference_cycle.format("%Y-%m-%dT%H:00:00Z").to_string(),
                "source_cycles":[reference_cycle.format("%Y-%m-%dT%H:00:00Z").to_string(), latest_cycle.format("%Y-%m-%dT%H:00:00Z").to_string()],
                "stitch":"previous cycle f000-f024 followed by latest cycle f003-f024",
                "region":"JMA MSM-compatible bounds resampled to JIS X 0410 third-order regional mesh",
                "grid":"japan_mesh3",
                "grid_registration":"cell_center",
                "interpolation":"bilinear",
                "source_resolution_degrees":0.25,
                "geospatial_lat_resolution":1.0 / 120.0,
                "geospatial_lon_resolution":1.0 / 80.0,
                "mesh_origin_degrees":[0.0,100.0],
                "comment":"Spatial resampling only; no terrain correction or additional forecast skill. Positive-weight missing inputs produce NaN.",
                "geospatial_lat_min":latitudes.iter().copied().fold(f64::INFINITY, f64::min),
                "geospatial_lat_max":latitudes.iter().copied().fold(f64::NEG_INFINITY, f64::max),
                "geospatial_lon_min":longitudes.iter().copied().fold(f64::INFINITY, f64::min),
                "geospatial_lon_max":longitudes.iter().copied().fold(f64::NEG_INFINITY, f64::max)
            }),
        )?;
        let hours: Vec<i32> = forecast_hours.iter().map(|&v| v as i32).collect();
        ZarrWriter::write_coordinate(
            &path,
            "forecast_hour",
            &as_le_i32(&hours),
            metadata_array(&[hours.len()], &[hours.len()], "<i4", Value::Null, false),
            json!({"_ARRAY_DIMENSIONS":["forecast_hour"],"units":"hours"}),
        )?;
        let times: Vec<i64> = forecast_hours
            .iter()
            .map(|&hour| {
                (reference_cycle + Duration::hours(hour as i64))
                    .timestamp_nanos_opt()
                    .unwrap()
            })
            .collect();
        ZarrWriter::write_coordinate(
            &path,
            "valid_time",
            &as_le_i64(&times),
            metadata_array(&[times.len()], &[times.len()], "<i8", Value::Null, false),
            json!({"_ARRAY_DIMENSIONS":["forecast_hour"],"standard_name":"time","units":"nanoseconds since 1970-01-01T00:00:00Z"}),
        )?;
        ZarrWriter::write_coordinate(
            &path,
            "latitude",
            &as_le_f64(latitudes),
            metadata_array(
                &[latitudes.len()],
                &[latitudes.len()],
                "<f8",
                Value::Null,
                false,
            ),
            json!({"_ARRAY_DIMENSIONS":["latitude"],"standard_name":"latitude","units":"degrees_north","bounds":"latitude_bounds"}),
        )?;
        ZarrWriter::write_coordinate(
            &path,
            "longitude",
            &as_le_f64(longitudes),
            metadata_array(
                &[longitudes.len()],
                &[longitudes.len()],
                "<f8",
                Value::Null,
                false,
            ),
            json!({"_ARRAY_DIMENSIONS":["longitude"],"standard_name":"longitude","units":"degrees_east","bounds":"longitude_bounds"}),
        )?;
        for (name, coordinates, origin, divisions) in [
            ("latitude", latitudes, 0.0, 120.0),
            ("longitude", longitudes, 100.0, 80.0),
        ] {
            let directory = path.join(format!("{name}_bounds"));
            fs::create_dir_all(&directory)?;
            write_json(
                &directory.join(".zarray"),
                &metadata_array(
                    &[coordinates.len(), 2],
                    &[coordinates.len(), 2],
                    "<f8",
                    Value::Null,
                    false,
                ),
            )?;
            write_json(
                &directory.join(".zattrs"),
                &json!({"_ARRAY_DIMENSIONS":[name,"bounds"]}),
            )?;
            let bounds: Vec<f64> = coordinates
                .iter()
                .flat_map(|&value| {
                    let index = ((value - origin) * divisions - 0.5).round();
                    let lower = origin + index / divisions;
                    let upper = origin + (index + 1.0) / divisions;
                    // CF bounds follow the coordinate direction. Integer mesh
                    // edges also make adjacent cells share identical bounds.
                    if name == "latitude" {
                        [upper, lower]
                    } else {
                        [lower, upper]
                    }
                })
                .collect();
            fs::write(directory.join("0.0"), as_le_f64(&bounds))?;
        }
        Ok(Self {
            path,
            forecast_count: hours.len(),
            nj: latitudes.len(),
            ni: longitudes.len(),
            arrays: HashSet::new(),
        })
    }

    pub fn write_slice(
        &mut self,
        name: &str,
        forecast_index: usize,
        values: &[f32],
        attrs: &Value,
    ) -> Result<Vec<PathBuf>> {
        ensure!(
            forecast_index < self.forecast_count,
            "regional forecast index is out of range"
        );
        ensure!(
            values.len() == self.nj * self.ni,
            "regional slice size mismatch"
        );
        let directory = self.path.join(name);
        if self.arrays.insert(name.to_owned()) {
            fs::create_dir_all(&directory)?;
            write_json(
                &directory.join(".zarray"),
                &metadata_array(
                    &[self.forecast_count, self.nj, self.ni],
                    &[1, self.nj.min(512), self.ni.min(512)],
                    "<f4",
                    json!("NaN"),
                    true,
                ),
            )?;
            let mut attrs = attrs.clone();
            attrs["interpolation"] = json!("bilinear");
            attrs["source_resolution_degrees"] = json!(0.25);
            write_json(&directory.join(".zattrs"), &attrs)?;
        }
        let chunk_j = self.nj.min(512);
        let chunk_i = self.ni.min(512);
        let mut paths = Vec::new();
        for cj in 0..self.nj.div_ceil(chunk_j) {
            for ci in 0..self.ni.div_ceil(chunk_i) {
                let mut chunk = vec![f32::NAN; chunk_j * chunk_i];
                for local_j in 0..chunk_j {
                    let source_j = cj * chunk_j + local_j;
                    if source_j >= self.nj {
                        break;
                    }
                    let count = chunk_i.min(self.ni - ci * chunk_i);
                    let source = source_j * self.ni + ci * chunk_i;
                    let target = local_j * chunk_i;
                    chunk[target..target + count].copy_from_slice(&values[source..source + count]);
                }
                let compressed = blosc_zstd_bitshuffle(&as_le_f32(&chunk), 4)?;
                let path = directory.join(format!("{forecast_index}.{cj}.{ci}"));
                fs::write(&path, compressed)?;
                paths.push(path);
            }
        }
        Ok(paths)
    }

    pub fn consolidate_metadata(&self) -> Result<()> {
        let writer = ZarrWriter {
            path: self.path.clone(),
            forecast_count: self.forecast_count,
            grid: None,
            arrays: HashSet::new(),
        };
        writer.consolidate_metadata()
    }
}

pub struct ZarrWriter {
    pub path: PathBuf,
    forecast_count: usize,
    grid: Option<(usize, usize)>,
    arrays: HashSet<String>,
}

impl ZarrWriter {
    pub fn new(path: PathBuf, cycle: DateTime<Utc>, forecast_hours: &[u32]) -> Result<Self> {
        fs::create_dir_all(&path)?;
        write_json(&path.join(".zgroup"), &json!({"zarr_format":2}))?;
        write_json(
            &path.join(".zattrs"),
            &json!({"Conventions":"CF-1.10", "cycle":cycle.format("%Y-%m-%dT%H:00:00Z").to_string(),
            "source":"NOAA NOMADS GFS pgrb2.0p25", "title":"NOAA NCEP GFS 0.25 degree selected fields"}),
        )?;
        let hours: Vec<i32> = forecast_hours.iter().map(|&v| v as i32).collect();
        Self::write_coordinate(
            &path,
            "forecast_hour",
            &as_le_i32(&hours),
            metadata_array(&[hours.len()], &[hours.len()], "<i4", json!(0), false),
            json!({"_ARRAY_DIMENSIONS":["forecast_hour"],"units":"hours"}),
        )?;
        let times: Vec<i64> = forecast_hours
            .iter()
            .map(|&h| {
                (cycle + Duration::hours(h as i64))
                    .timestamp_nanos_opt()
                    .unwrap()
            })
            .collect();
        Self::write_coordinate(
            &path,
            "valid_time",
            &as_le_i64(&times),
            metadata_array(&[times.len()], &[times.len()], "<i8", json!(0), false),
            json!({"_ARRAY_DIMENSIONS":["forecast_hour"],"standard_name":"time","units":"nanoseconds since 1970-01-01T00:00:00Z"}),
        )?;
        Ok(Self {
            path,
            forecast_count: hours.len(),
            grid: None,
            arrays: HashSet::new(),
        })
    }

    fn write_coordinate(
        root: &Path,
        name: &str,
        bytes: &[u8],
        zarray: Value,
        attrs: Value,
    ) -> Result<()> {
        let directory = root.join(name);
        fs::create_dir_all(&directory)?;
        write_json(&directory.join(".zarray"), &zarray)?;
        write_json(&directory.join(".zattrs"), &attrs)?;
        fs::write(directory.join("0"), bytes)?;
        Ok(())
    }

    fn ensure_coordinates(&mut self, message: &GribMessage, nj: usize, ni: usize) -> Result<()> {
        if let Some(grid) = self.grid {
            ensure!(
                grid == (nj, ni),
                "selected GRIB messages use different grids"
            );
            return Ok(());
        }
        let lat = message.array("latitudes")?;
        let lon = message.array("longitudes")?;
        ensure!(
            lat.len() == nj * ni && lon.len() == nj * ni,
            "GRIB coordinate array size mismatch"
        );
        let lat: Vec<f32> = (0..nj).map(|j| lat[j * ni] as f32).collect();
        let lon: Vec<f32> = (0..ni).map(|i| lon[i] as f32).collect();
        Self::write_coordinate(
            &self.path,
            "latitude",
            &as_le_f32(&lat),
            metadata_array(&[nj], &[nj.min(721)], "<f4", Value::Null, false),
            json!({"_ARRAY_DIMENSIONS":["latitude"],"standard_name":"latitude","units":"degrees_north"}),
        )?;
        Self::write_coordinate(
            &self.path,
            "longitude",
            &as_le_f32(&lon),
            metadata_array(&[ni], &[ni.min(1440)], "<f4", Value::Null, false),
            json!({"_ARRAY_DIMENSIONS":["longitude"],"standard_name":"longitude","units":"degrees_east"}),
        )?;
        self.grid = Some((nj, ni));
        Ok(())
    }

    pub fn write_message(
        &mut self,
        selection: &Selection,
        forecast_index: usize,
        bytes: &[u8],
    ) -> Result<()> {
        let message = GribMessage::decode(bytes).with_context(|| selection.zarr_name.clone())?;
        let ni = message.long("Ni")? as usize;
        let nj = message.long("Nj")? as usize;
        self.ensure_coordinates(&message, nj, ni)?;
        let missing = message.double("missingValue")?;
        let values: Vec<f32> = message
            .array("values")?
            .into_iter()
            .map(|value| {
                if value == missing {
                    f32::NAN
                } else {
                    value as f32
                }
            })
            .collect();
        ensure!(values.len() == nj * ni, "GRIB value array size mismatch");
        let directory = self.path.join(&selection.zarr_name);
        if self.arrays.insert(selection.zarr_name.clone()) {
            fs::create_dir_all(&directory)?;
            write_json(
                &directory.join(".zarray"),
                &metadata_array(
                    &[self.forecast_count, nj, ni],
                    &[1, nj.min(361), ni.min(720)],
                    "<f4",
                    json!("NaN"),
                    true,
                ),
            )?;
            write_json(
                &directory.join(".zattrs"),
                &json!({"_ARRAY_DIMENSIONS":["forecast_hour","latitude","longitude"],
                "common_name_ja":selection.common_name_ja,"coordinates":"valid_time","level":selection.level,
                "long_name":message.string("name")?,"short_name":selection.short_name,"statistic":selection.statistic,
                "units":message.string("units")?}),
            )?;
        }
        let chunk_j = nj.min(361);
        let chunk_i = ni.min(720);
        for cj in 0..nj.div_ceil(chunk_j) {
            for ci in 0..ni.div_ceil(chunk_i) {
                let mut chunk = vec![f32::NAN; chunk_j * chunk_i];
                for local_j in 0..chunk_j {
                    let source_j = cj * chunk_j + local_j;
                    if source_j >= nj {
                        break;
                    }
                    let count = chunk_i.min(ni - ci * chunk_i);
                    let source = source_j * ni + ci * chunk_i;
                    let target = local_j * chunk_i;
                    chunk[target..target + count].copy_from_slice(&values[source..source + count]);
                }
                let compressed = blosc_zstd_bitshuffle(&as_le_f32(&chunk), 4)?;
                fs::write(
                    directory.join(format!("{forecast_index}.{cj}.{ci}")),
                    compressed,
                )?;
            }
        }
        Ok(())
    }

    pub fn consolidate_metadata(&self) -> Result<()> {
        let mut metadata = BTreeMap::new();
        for entry in walkdir::WalkDir::new(&self.path) {
            let entry = entry?;
            if !entry.file_type().is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy();
            if name != ".zarray" && name != ".zattrs" && name != ".zgroup" {
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(&self.path)?
                .to_string_lossy()
                .replace('\\', "/");
            metadata.insert(
                relative,
                serde_json::from_slice::<Value>(&fs::read(entry.path())?)?,
            );
        }
        write_json(
            &self.path.join(".zmetadata"),
            &json!({"metadata":metadata,"zarr_consolidated_format":1}),
        )
    }
}

pub fn completion_marker(cycle: DateTime<Utc>, objects: usize) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(
        &json!({"cycle":cycle.to_rfc3339(),"objects":objects}),
    )?)
}

pub fn collect_files(directory: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(directory) {
        let entry = entry?;
        if entry.file_type().is_file() {
            files.push(entry.into_path());
        }
    }
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn settings() -> Settings {
        Settings {
            source_base_url: "https://example.test/prod".into(),
            source_age_hours: 24,
            cycle: "auto".into(),
            forecast_hours: vec![0, 3],
            selection_file: "fields.csv".into(),
            s3_endpoint_url: Some("http://s3".into()),
            s3_region: "us-east-1".into(),
            s3_bucket: "weather".into(),
            s3_prefix: "noaa-gfs".into(),
            forecast_enabled: true,
            forecast_prefix: "forecast".into(),
            forecast_lat_min: 22.4,
            forecast_lat_max: 47.6,
            forecast_lon_min: 120.0,
            forecast_lon_max: 150.0,
            retention_days: 7,
            overwrite: false,
            download_retries: 4,
        }
    }

    #[test]
    fn forecast_hours() {
        assert_eq!(
            parse_forecast_hours("000,3-9:3,012").unwrap(),
            vec![0, 3, 6, 9, 12]
        );
        assert!(parse_forecast_hours("385").is_err());
    }
    #[test]
    fn four_daily_runs_select_all_previous_day_cycles() {
        for (run_hour, cycle_hour) in [(4, 0), (10, 6), (16, 12), (22, 18)] {
            let now = format!("2026-08-25T{run_hour:02}:15:00Z").parse().unwrap();
            let expected = format!("2026-08-24T{cycle_hour:02}:00:00Z")
                .parse::<DateTime<Utc>>()
                .unwrap();
            assert_eq!(select_cycle(now, 24, "auto").unwrap(), expected);
        }
    }
    #[test]
    fn urls() {
        let cycle = "2026-08-24T06:00:00Z".parse().unwrap();
        assert_eq!(
            source_url(&settings(), cycle, 12),
            "https://example.test/prod/gfs.20260824/06/atmos/gfs.t06z.pgrb2.0p25.f012"
        );
        assert_eq!(
            zarr_object_prefix(&settings(), cycle),
            "noaa-gfs/2026082406.zarr"
        );
        assert_eq!(
            forecast_object_prefix(&settings(), cycle),
            "forecast/2026082406.zarr"
        );
    }
    #[test]
    fn statistics() {
        assert_eq!(statistic("anl"), "instant");
        assert_eq!(statistic("3 hour fcst"), "instant");
        assert_eq!(statistic("0-3 hour acc fcst"), "accumulation");
        assert_eq!(statistic("0-3 hour ave fcst"), "average");
    }
    #[test]
    fn index_ranges() {
        let entries=parse_index("1:0:d=2026082406:TMP:surface:3 hour fcst:\n2:100:d=2026082406:APCP:surface:0-3 hour acc fcst:\n3:250:d=2026082406:APCP:surface:0-3 hour acc fcst:\n").unwrap();
        assert_eq!((entries[0].start, entries[0].end), (0, Some(99)));
        assert_eq!((entries[1].start, entries[1].end), (100, Some(249)));
        assert_eq!(entries[2].end, None);
        assert_eq!((entries[1].occurrence, entries[2].occurrence), (1, 2));
    }
    #[test]
    fn commented_selection_is_disabled() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file,"## comment\nshort_name,level,statistic,occurrence,zarr_name,common_name_ja\nTMP,2 m above ground,instant,1,air_temperature_2m,地上2m気温\n#RH,2 m above ground,instant,1,relative_humidity_2m,地上2m相対湿度\n").unwrap();
        let items = load_selections(file.path()).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].zarr_name, "air_temperature_2m");
    }
    #[test]
    fn booleans() {
        assert!(parse_bool("true").unwrap());
        assert!(!parse_bool("0").unwrap());
        assert!(parse_bool("sometimes").is_err());
    }
    #[test]
    fn blosc_round_trip() {
        let input: Vec<u8> = (0_u32..4096).flat_map(u32::to_le_bytes).collect();
        let compressed = blosc_zstd_bitshuffle(&input, 4).unwrap();
        assert_eq!(blosc_decompress(&compressed, input.len()).unwrap(), input);
    }
    #[test]
    fn zarr_metadata_is_consolidated() {
        let temp = tempfile::tempdir().unwrap();
        let cycle = "2026-08-24T06:00:00Z".parse().unwrap();
        let writer = ZarrWriter::new(temp.path().join("x.zarr"), cycle, &[0, 3]).unwrap();
        writer.consolidate_metadata().unwrap();
        let metadata: Value =
            serde_json::from_slice(&fs::read(writer.path.join(".zmetadata")).unwrap()).unwrap();
        assert_eq!(
            metadata["metadata"]["forecast_hour/.zarray"]["dtype"],
            "<i4"
        );
    }

    #[test]
    fn regional_zarr_has_mesh_centers_bounds_and_padded_chunks() {
        let temp = tempfile::tempdir().unwrap();
        let cycle: DateTime<Utc> = "2026-10-01T00:00:00Z".parse().unwrap();
        let latitudes: Vec<f64> = (0..513).map(|j| (4500.5 - j as f64) / 120.0).collect();
        let longitudes: Vec<f64> = (0..514)
            .map(|i| 100.0 + (3000.5 + i as f64) / 80.0)
            .collect();
        let mut writer = RegionalZarrWriter::new(
            temp.path().join("regional.zarr"),
            cycle,
            cycle + Duration::hours(24),
            &[0, 3],
            &latitudes,
            &longitudes,
        )
        .unwrap();
        let mut values: Vec<f32> = (0..513)
            .flat_map(|j| (0..514).map(move |i| (j * 1000 + i) as f32))
            .collect();
        values[0] = f32::NAN;
        let attrs =
            json!({"_ARRAY_DIMENSIONS":["forecast_hour","latitude","longitude"],"units":"K"});
        let chunks = writer
            .write_slice("temperature", 1, &values, &attrs)
            .unwrap();
        assert_eq!(chunks.len(), 4);
        let last = blosc_decompress(
            &fs::read(writer.path.join("temperature/1.1.1")).unwrap(),
            512 * 512 * 4,
        )
        .unwrap();
        let read_f32 = |offset| f32::from_le_bytes(last[offset..offset + 4].try_into().unwrap());
        assert_eq!(read_f32(0), 512512.0);
        assert_eq!(read_f32(4), 512513.0);
        assert!(read_f32(8).is_nan());
        assert!(read_f32(512 * 4).is_nan());
        let lat_bytes = fs::read(writer.path.join("latitude/0")).unwrap();
        assert_eq!(
            f64::from_le_bytes(lat_bytes[0..8].try_into().unwrap()),
            latitudes[0]
        );
        let bounds = fs::read(writer.path.join("longitude_bounds/0.0")).unwrap();
        assert!((f64::from_le_bytes(bounds[0..8].try_into().unwrap()) - 137.5).abs() < 1e-12);
        let lat_bounds = fs::read(writer.path.join("latitude_bounds/0.0")).unwrap();
        let boundary =
            |offset| f64::from_le_bytes(lat_bounds[offset..offset + 8].try_into().unwrap());
        assert!(boundary(0) > boundary(8));
        assert_eq!(boundary(8), boundary(16));
        // Simulate streamed chunk removal: metadata must still consolidate.
        for path in chunks {
            fs::remove_file(path).unwrap();
        }
        writer.consolidate_metadata().unwrap();
        let metadata: Value =
            serde_json::from_slice(&fs::read(writer.path.join(".zmetadata")).unwrap()).unwrap();
        assert_eq!(
            metadata["metadata"]["temperature/.zarray"]["shape"],
            json!([2, 513, 514])
        );
        assert_eq!(
            metadata["metadata"]["temperature/.zarray"]["chunks"],
            json!([1, 512, 512])
        );
        assert_eq!(metadata["metadata"]["latitude/.zarray"]["dtype"], "<f8");
        assert!(metadata["metadata"]["forecast_hour/.zarray"]["fill_value"].is_null());
        assert_eq!(
            metadata["metadata"]["longitude/.zattrs"]["bounds"],
            "longitude_bounds"
        );
        assert_eq!(
            metadata["metadata"][".zattrs"]["grid_registration"],
            "cell_center"
        );
    }

    #[test]
    #[ignore = "requires a real GRIB2 message path in GFS_TEST_GRIB_FILE"]
    fn real_grib_message_decodes_and_writes_zarr() {
        let input = std::env::var("GFS_TEST_GRIB_FILE").unwrap();
        let bytes = fs::read(input).unwrap();
        let message = GribMessage::decode(&bytes).unwrap();
        assert_eq!(
            (message.long("Nj").unwrap(), message.long("Ni").unwrap()),
            (721, 1440)
        );
        let temp = tempfile::tempdir().unwrap();
        let cycle = "2026-08-26T00:00:00Z".parse().unwrap();
        let mut writer = ZarrWriter::new(temp.path().join("x.zarr"), cycle, &[3]).unwrap();
        let selection = Selection {
            short_name: "TMP".into(),
            level: "2 m above ground".into(),
            statistic: "instant".into(),
            occurrence: 1,
            zarr_name: "air_temperature_2m".into(),
            common_name_ja: "地上2m気温".into(),
        };
        writer.write_message(&selection, 0, &bytes).unwrap();
        writer.consolidate_metadata().unwrap();
        let chunks = fs::read_dir(writer.path.join("air_temperature_2m"))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
            .count();
        assert_eq!(chunks, 4);
        assert!(writer.path.join(".zmetadata").is_file());
    }
}
