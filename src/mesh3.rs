//! Resample geographic point samples onto Japanese third-order mesh cell centers.
//!
//! The mesh spacing is 30 arc-seconds of latitude and 45 arc-seconds of longitude.
//! This is interpolation of the source forecast, not additional forecast detail.

use anyhow::{Context, Result, ensure};

// Bound the largest single f32 output to 128 MB. The default Japan extent uses
// 7,257,600 cells (29,030,400 bytes); mistakenly requesting the globe is rejected.
const MAX_GRID_CELLS: usize = 32_000_000;

#[derive(Debug)]
struct AxisWeight {
    first: usize,
    second_weight: f64,
}

#[derive(Debug)]
pub struct Mesh3Grid {
    /// Cell centers ordered from north to south, in degrees north.
    pub latitudes: Vec<f64>,
    /// Cell centers ordered from west to east, in degrees east.
    pub longitudes: Vec<f64>,
    /// Inclusive source row bounds, including the interpolation halo.
    pub j_range: (usize, usize),
    /// Inclusive source column bounds, including the interpolation halo.
    pub i_range: (usize, usize),
    latitude_weights: Vec<AxisWeight>,
    longitude_weights: Vec<AxisWeight>,
    source_width: usize,
    source_cells: usize,
    output_cells: usize,
}

impl Mesh3Grid {
    /// Include every mesh cell whose center falls within the inclusive bounds.
    /// Source axes must be finite and strictly monotonic; either direction works.
    /// Every target center must be bracketed by source samples (no extrapolation).
    pub fn new(
        source_latitudes: &[f32],
        source_longitudes: &[f32],
        lat_min: f64,
        lat_max: f64,
        lon_min: f64,
        lon_max: f64,
    ) -> Result<Self> {
        validate_bounds(lat_min, lat_max, -90.0, 90.0, "latitude")?;
        validate_bounds(lon_min, lon_max, -180.0, 180.0, "longitude")?;
        let latitude_ascending = validate_axis(source_latitudes, "latitude")?;
        let longitude_ascending = validate_axis(source_longitudes, "longitude")?;

        // Derive each coordinate from its integer mesh index. Repeatedly adding
        // 1/120 or 1/80 would accumulate error and shift mesh boundary selection.
        let mut latitudes = mesh_centers(lat_min, lat_max, 0.0, 120.0, "latitude")?;
        latitudes.reverse();
        let longitudes = mesh_centers(lon_min, lon_max, 100.0, 80.0, "longitude")?;
        let output_cells = checked_cells(latitudes.len(), longitudes.len(), "output")?;

        let (latitude_weights, j_range) =
            interpolation_weights(source_latitudes, &latitudes, latitude_ascending, "latitude")?;
        let (longitude_weights, i_range) = interpolation_weights(
            source_longitudes,
            &longitudes,
            longitude_ascending,
            "longitude",
        )?;
        let source_width = i_range.1 - i_range.0 + 1;
        let source_cells = checked_cells(j_range.1 - j_range.0 + 1, source_width, "source")?;

        Ok(Self {
            latitudes,
            longitudes,
            j_range,
            i_range,
            latitude_weights,
            longitude_weights,
            source_width,
            source_cells,
            output_cells,
        })
    }

    /// Bilinearly interpolate a row-major rectangle extracted with j_range and
    /// i_range, preserving the source axis order within that rectangle.
    /// A missing/non-finite sample propagates only when its weight is nonzero.
    pub fn interpolate(&self, source: &[f32]) -> Result<Vec<f32>> {
        ensure!(
            source.len() == self.source_cells,
            "mesh3 source slice has {} values, expected {} for the inclusive source ranges",
            source.len(),
            self.source_cells
        );
        let mut output = Vec::new();
        output
            .try_reserve_exact(self.output_cells)
            .context("allocate mesh3 interpolation output")?;

        for latitude in &self.latitude_weights {
            let first_row = latitude.first * self.source_width;
            let second_row = first_row + self.source_width;
            let y = latitude.second_weight;
            for longitude in &self.longitude_weights {
                let x = longitude.second_weight;
                let first_column = longitude.first;
                let samples = [
                    (source[first_row + first_column], (1.0 - y) * (1.0 - x)),
                    (source[first_row + first_column + 1], (1.0 - y) * x),
                    (source[second_row + first_column], y * (1.0 - x)),
                    (source[second_row + first_column + 1], y * x),
                ];
                let mut value = 0.0_f64;
                for (sample, weight) in samples {
                    // In particular, 0 * NaN must not contaminate an exact
                    // source-grid point or interpolation along a source edge.
                    if weight == 0.0 {
                        continue;
                    }
                    if !sample.is_finite() {
                        value = f64::NAN;
                        break;
                    }
                    value += f64::from(sample) * weight;
                }
                output.push(value as f32);
            }
        }
        Ok(output)
    }
}

fn validate_bounds(min: f64, max: f64, floor: f64, ceiling: f64, axis: &str) -> Result<()> {
    ensure!(
        min.is_finite() && max.is_finite(),
        "mesh3 {axis} bounds must be finite"
    );
    ensure!(min <= max, "mesh3 {axis} minimum exceeds maximum");
    ensure!(
        min >= floor && max <= ceiling,
        "mesh3 {axis} bounds must be within [{floor}, {ceiling}]"
    );
    Ok(())
}

fn validate_axis(values: &[f32], axis: &str) -> Result<bool> {
    ensure!(values.len() >= 2, "source {axis} needs at least two points");
    ensure!(
        values.iter().all(|value| value.is_finite()),
        "source {axis} coordinates must be finite"
    );
    let ascending = values[0] < values[1];
    ensure!(
        values.windows(2).all(|pair| if ascending {
            pair[0] < pair[1]
        } else {
            pair[0] > pair[1]
        }),
        "source {axis} coordinates must be strictly monotonic"
    );
    Ok(ascending)
}

fn mesh_centers(min: f64, max: f64, origin: f64, divisions: f64, axis: &str) -> Result<Vec<f64>> {
    // Add one candidate on each side and then compare actual centers. This
    // preserves inclusive bounds even if multiplying a supplied center rounds
    // its theoretical integer mesh index slightly up or down.
    let first = ((min - origin) * divisions - 0.5).ceil() as i64 - 1;
    let last = ((max - origin) * divisions - 0.5).floor() as i64 + 1;
    let values: Vec<f64> = (first..=last)
        .map(|index| origin + (index as f64 + 0.5) / divisions)
        .filter(|&center| center >= min && center <= max)
        .collect();
    ensure!(
        !values.is_empty(),
        "mesh3 {axis} bounds contain no cell centers"
    );
    Ok(values)
}

fn checked_cells(rows: usize, columns: usize, kind: &str) -> Result<usize> {
    let cells = rows
        .checked_mul(columns)
        .context("mesh3 grid dimensions overflow")?;
    ensure!(
        cells <= MAX_GRID_CELLS,
        "mesh3 {kind} grid has {cells} cells; maximum is {MAX_GRID_CELLS}"
    );
    Ok(cells)
}

fn interpolation_weights(
    source: &[f32],
    target: &[f64],
    ascending: bool,
    axis: &str,
) -> Result<(Vec<AxisWeight>, (usize, usize))> {
    let first = f64::from(source[0]);
    let last = f64::from(source[source.len() - 1]);
    let min = first.min(last);
    let max = first.max(last);
    let mut weights = Vec::with_capacity(target.len());
    let mut start = usize::MAX;
    let mut end = 0;
    for &coordinate in target {
        ensure!(
            coordinate >= min && coordinate <= max,
            "mesh3 {axis} center {coordinate} lies outside source [{min}, {max}]; extrapolation is disabled"
        );
        let right = source.partition_point(|&value| {
            if ascending {
                f64::from(value) < coordinate
            } else {
                f64::from(value) > coordinate
            }
        });
        let left = right.saturating_sub(1).min(source.len() - 2);
        let a = f64::from(source[left]);
        let b = f64::from(source[left + 1]);
        let second_weight = (coordinate - a) / (b - a);
        ensure!(
            (0.0..=1.0).contains(&second_weight),
            "invalid mesh3 {axis} interpolation weight"
        );
        start = start.min(left);
        end = end.max(left + 1);
        weights.push(AxisWeight {
            first: left,
            second_weight,
        });
    }
    for weight in &mut weights {
        weight.first -= start;
    }
    Ok((weights, (start, end)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gfs_axes() -> (Vec<f32>, Vec<f32>) {
        (
            (0..=720).map(|j| 90.0 - j as f32 * 0.25).collect(),
            (0..1440).map(|i| i as f32 * 0.25).collect(),
        )
    }

    fn extract_function(
        grid: &Mesh3Grid,
        latitudes: &[f32],
        longitudes: &[f32],
        f: impl Fn(f32, f32) -> f32,
    ) -> Vec<f32> {
        let mut values = Vec::new();
        for &latitude in &latitudes[grid.j_range.0..=grid.j_range.1] {
            for &longitude in &longitudes[grid.i_range.0..=grid.i_range.1] {
                values.push(f(latitude, longitude));
            }
        }
        values
    }

    #[test]
    fn default_japan_extent_has_exact_mesh_dimensions_and_halo() {
        let (latitudes, longitudes) = gfs_axes();
        let grid = Mesh3Grid::new(&latitudes, &longitudes, 22.4, 47.6, 120.0, 150.0).unwrap();
        assert_eq!(grid.latitudes.len(), 3024);
        assert_eq!(grid.longitudes.len(), 2400);
        assert_eq!(grid.output_cells, 7_257_600);
        assert_eq!(grid.j_range, (169, 271));
        assert_eq!(grid.i_range, (480, 600));
        assert_eq!(grid.latitudes[0], (5711.0 + 0.5) / 120.0);
        assert_eq!(*grid.latitudes.last().unwrap(), (2688.0 + 0.5) / 120.0);
        assert_eq!(grid.longitudes[0], 100.0 + (1600.0 + 0.5) / 80.0);
        assert_eq!(
            *grid.longitudes.last().unwrap(),
            100.0 + (3999.0 + 0.5) / 80.0
        );
        assert!(grid.latitudes.windows(2).all(|pair| pair[0] > pair[1]));
        assert!(grid.longitudes.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn tokyo_mesh_center_is_selected_at_inclusive_bounds() {
        let (latitudes, longitudes) = gfs_axes();
        // Third-order mesh 53394527: 35 degrees 41 minutes 15 seconds north,
        // 139 degrees 43 minutes 7.5 seconds east.
        let grid = Mesh3Grid::new(
            &latitudes,
            &longitudes,
            35.6875,
            35.6875,
            139.71875,
            139.71875,
        )
        .unwrap();
        assert_eq!(grid.latitudes, vec![35.6875]);
        assert_eq!(grid.longitudes, vec![139.71875]);

        // Also exercise centers that do not have an exact binary representation.
        let latitude = (4280.0 + 0.5) / 120.0;
        let longitude = 100.0 + (3175.0 + 0.5) / 80.0;
        let grid = Mesh3Grid::new(
            &latitudes,
            &longitudes,
            latitude,
            latitude,
            longitude,
            longitude,
        )
        .unwrap();
        assert_eq!(grid.latitudes, vec![latitude]);
        assert_eq!(grid.longitudes, vec![longitude]);
    }

    #[test]
    fn bilinear_interpolation_reproduces_affine_field_in_either_axis_order() {
        for reverse_latitude in [false, true] {
            for reverse_longitude in [false, true] {
                let mut latitudes = vec![36.0, 35.75, 35.5];
                let mut longitudes = vec![139.5, 139.75, 140.0];
                if reverse_latitude {
                    latitudes.reverse();
                }
                if reverse_longitude {
                    longitudes.reverse();
                }
                let grid =
                    Mesh3Grid::new(&latitudes, &longitudes, 35.6, 35.9, 139.6, 139.9).unwrap();
                let source = extract_function(&grid, &latitudes, &longitudes, |y, x| {
                    2.0 * y + 3.0 * x - 400.0
                });
                let output = grid.interpolate(&source).unwrap();
                for (j, &latitude) in grid.latitudes.iter().enumerate() {
                    for (i, &longitude) in grid.longitudes.iter().enumerate() {
                        let expected = (2.0 * latitude + 3.0 * longitude - 400.0) as f32;
                        assert!((output[j * grid.longitudes.len() + i] - expected).abs() < 0.0001);
                    }
                }
            }
        }
    }

    #[test]
    fn exact_source_endpoints_ignore_zero_weight_missing_neighbors() {
        let latitudes = [35.6875, 35.4375];
        let longitudes = [139.71875, 139.96875];
        for (j, &latitude) in latitudes.iter().enumerate() {
            for (i, &longitude) in longitudes.iter().enumerate() {
                let grid = Mesh3Grid::new(
                    &latitudes,
                    &longitudes,
                    f64::from(latitude),
                    f64::from(latitude),
                    f64::from(longitude),
                    f64::from(longitude),
                )
                .unwrap();
                let mut source = [f32::NAN; 4];
                source[j * 2 + i] = 42.0;
                assert_eq!(grid.interpolate(&source).unwrap(), vec![42.0]);
            }
        }
    }

    #[test]
    fn missing_values_propagate_only_from_nonzero_weights() {
        let edge = Mesh3Grid::new(
            &[35.6875, 35.4375],
            &[139.59375, 139.84375],
            35.6875,
            35.6875,
            139.71875,
            139.71875,
        )
        .unwrap();
        assert_eq!(
            edge.interpolate(&[2.0, 4.0, f32::NAN, f32::NAN]).unwrap(),
            vec![3.0]
        );
        assert!(edge.interpolate(&[2.0, f32::NAN, 3.0, 4.0]).unwrap()[0].is_nan());

        let center = Mesh3Grid::new(
            &[35.8125, 35.5625],
            &[139.59375, 139.84375],
            35.6875,
            35.6875,
            139.71875,
            139.71875,
        )
        .unwrap();
        assert_eq!(
            center.interpolate(&[2.0, 4.0, 6.0, 8.0]).unwrap(),
            vec![5.0]
        );
        for index in 0..4 {
            let mut source = [2.0, 4.0, 6.0, 8.0];
            source[index] = f32::NAN;
            assert!(center.interpolate(&source).unwrap()[0].is_nan());
        }
    }

    #[test]
    fn tiny_region_reads_bracketing_source_points_outside_requested_bounds() {
        let grid = Mesh3Grid::new(
            &[36.0, 35.75, 35.5, 35.25, 35.0],
            &[139.0, 139.25, 139.5, 139.75, 140.0],
            35.61,
            35.62,
            139.61,
            139.62,
        )
        .unwrap();
        assert_eq!(grid.j_range, (1, 2));
        assert_eq!(grid.i_range, (2, 3));
        assert_eq!(grid.latitudes.len(), 1);
        assert_eq!(grid.longitudes.len(), 1);
        let expected = 2.0
            + 4.0 * ((35.75 - grid.latitudes[0]) / 0.25)
            + 2.0 * ((grid.longitudes[0] - 139.5) / 0.25);
        assert!(
            (f64::from(grid.interpolate(&[2.0, 4.0, 6.0, 8.0]).unwrap()[0]) - expected).abs()
                < 0.0001
        );
        assert!(grid.interpolate(&[1.0, 2.0, 3.0]).is_err());
    }

    #[test]
    fn rejects_invalid_coordinates_bounds_extrapolation_and_large_allocations() {
        let latitudes = [36.0, 35.0];
        let longitudes = [139.0, 140.0];
        for bad_latitudes in [
            vec![],
            vec![36.0],
            vec![36.0, 36.0],
            vec![36.0, 35.0, 35.5],
            vec![36.0, f32::NAN],
        ] {
            assert!(Mesh3Grid::new(&bad_latitudes, &longitudes, 35.6, 35.7, 139.6, 139.7).is_err());
        }
        for bad_longitudes in [vec![139.0], vec![139.0, 139.0], vec![139.0, f32::INFINITY]] {
            assert!(Mesh3Grid::new(&latitudes, &bad_longitudes, 35.6, 35.7, 139.6, 139.7).is_err());
        }
        for (min, max) in [
            (f64::NAN, 35.7),
            (35.6, f64::INFINITY),
            (35.7, 35.6),
            (-91.0, 35.7),
            (35.68, 35.681),
            (36.1, 36.2),
        ] {
            assert!(Mesh3Grid::new(&latitudes, &longitudes, min, max, 139.6, 139.7).is_err());
        }
        for (min, max) in [
            (f64::NAN, 140.0),
            (139.0, 181.0),
            (140.0, 139.0),
            (140.1, 140.2),
        ] {
            assert!(Mesh3Grid::new(&latitudes, &longitudes, 35.6, 35.7, min, max).is_err());
        }
        assert!(
            Mesh3Grid::new(&[90.0, -90.0], &[-180.0, 180.0], -90.0, 90.0, -180.0, 180.0).is_err()
        );
    }
}
