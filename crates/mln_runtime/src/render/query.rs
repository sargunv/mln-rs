//! The feature queries of a render session.

use mln_util::error::Error;
use mln_util::geo::ScreenCoordinate;
use mln_util::size::Size;

/// The screen area that a rendered feature query covers, in logical pixels
/// from the top left corner.
#[derive(Clone, Debug, PartialEq)]
pub enum RenderedQueryGeometry {
    Point(ScreenCoordinate),
    /// The rectangle from its `min` corner to its `max` corner.
    Box {
        min: ScreenCoordinate,
        max: ScreenCoordinate,
    },
    /// A line through its points.
    LineString(Vec<ScreenCoordinate>),
}

impl RenderedQueryGeometry {
    /// The geometry as it covers a target of `size`: a box loses its parts
    /// outside, and one wholly outside covers nothing. The renderer's tile
    /// geometry saturates a few tiles past a tile's bounds, so a box that
    /// reached far past the target would find nothing.
    pub(crate) fn within(self, size: Size) -> Option<RenderedQueryGeometry> {
        let RenderedQueryGeometry::Box { min, max } = self else { return Some(self) };
        let (width, height) = (f64::from(size.width), f64::from(size.height));
        let (min_x, max_x) = (min.x.min(max.x), min.x.max(max.x));
        let (min_y, max_y) = (min.y.min(max.y), min.y.max(max.y));
        if min_x > width || min_y > height || max_x < 0.0 || max_y < 0.0 {
            return None;
        }
        Some(RenderedQueryGeometry::Box {
            min: ScreenCoordinate::new(min_x.max(0.0), min_y.max(0.0)),
            max: ScreenCoordinate::new(max_x.min(width), max_y.min(height)),
        })
    }

    /// The geometry's points, or why the query can't use it.
    pub(crate) fn points(&self) -> Result<Vec<ScreenCoordinate>, Error> {
        let points = match self {
            RenderedQueryGeometry::Point(point) => vec![*point],
            RenderedQueryGeometry::Box { min, max } => {
                vec![*min, ScreenCoordinate::new(max.x, min.y), *max, ScreenCoordinate::new(min.x, max.y), *min]
            }
            RenderedQueryGeometry::LineString(points) => {
                if points.is_empty() {
                    return Err(Error::invalid_argument("a query line string needs points"));
                }
                points.clone()
            }
        };
        if points.iter().any(|point| !point.x.is_finite() || !point.y.is_finite()) {
            return Err(Error::invalid_argument("a query point must be finite"));
        }
        Ok(points)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(x: f64, y: f64) -> ScreenCoordinate {
        ScreenCoordinate::new(x, y)
    }

    fn query_box(min: ScreenCoordinate, max: ScreenCoordinate) -> RenderedQueryGeometry {
        RenderedQueryGeometry::Box { min, max }
    }

    #[test]
    fn a_box_is_normalized_and_clipped_to_the_target() {
        let size = Size::new(64, 32);
        let beyond = query_box(point(-100.0, -100.0), point(100.0, 100.0));
        assert_eq!(beyond.within(size), Some(query_box(point(0.0, 0.0), point(64.0, 32.0))));
        let swapped = query_box(point(50.0, 30.0), point(10.0, 5.0));
        assert_eq!(swapped.within(size), Some(query_box(point(10.0, 5.0), point(50.0, 30.0))));
        assert_eq!(query_box(point(70.0, 0.0), point(90.0, 10.0)).within(size), None);
        let line = RenderedQueryGeometry::LineString(vec![point(-5.0, 0.0), point(500.0, 0.0)]);
        assert_eq!(line.clone().within(size), Some(line));
    }
}
