use crate::Detection;
use std::cmp::Ordering;

// https://learnopencv.com/non-maximum-suppression-theory-and-implementation-in-pytorch
pub fn non_max_suppression(mut detections: Vec<Detection>, iou_threshold: f32) -> Vec<Detection> {
    detections.sort_by(|a, b| a.score.partial_cmp(&b.score).unwrap_or(Ordering::Greater));
    let mut iterations = 0;
    let mut keep = Vec::new();
    while let Some(d) = detections.pop() {
        if iterations > 10000 {
            // This is a O(n²) algorithm in the very unlikely
            // worst case, so we need some type of limit.
            break;
        }
        detections.retain(|d2| {
            iterations += 1;
            iou(&d, d2) < iou_threshold
        });
        keep.push(d);
    }
    keep
}

// Intersection over union.
fn iou(a: &Detection, b: &Detection) -> f32 {
    let max_left = f32::max(a.left, b.left);
    let max_top = f32::max(a.top, b.top);
    let min_right = f32::min(a.right, b.right);
    let min_bottom = f32::min(a.bottom, b.bottom);

    let w = f32::max(0.0, min_right - max_left);
    let h = f32::max(0.0, min_bottom - max_top);

    let intersection_area = w * h;
    let union_area = a.area() + b.area() - intersection_area;

    intersection_area / union_area
}

#[cfg(test)]
mod tests {
    use super::*;

    fn det(score: f32, top: f32, left: f32, bottom: f32, right: f32) -> Detection {
        Detection {
            score,
            class: 0,
            top,
            left,
            bottom,
            right,
        }
    }

    #[test]
    fn test_nms_empty() {
        assert!(non_max_suppression(Vec::new(), 0.5).is_empty());
    }

    #[test]
    fn test_nms_single() {
        let out = non_max_suppression(vec![det(0.9, 0.1, 0.1, 0.5, 0.5)], 0.5);
        assert_eq!(1, out.len());
        assert!((out[0].score - 0.9).abs() < 1e-6);
    }

    // Two overlapping boxes: only the higher score survives.
    #[test]
    fn test_nms_suppresses_overlap() {
        let out = non_max_suppression(
            vec![
                det(0.5, 0.10, 0.10, 0.50, 0.50),
                det(0.9, 0.11, 0.11, 0.51, 0.51),
            ],
            0.6,
        );
        assert_eq!(1, out.len());
        assert!((out[0].score - 0.9).abs() < 1e-6);
    }

    // Disjoint boxes are both kept.
    #[test]
    fn test_nms_keeps_disjoint() {
        let out = non_max_suppression(
            vec![
                det(0.5, 0.0, 0.0, 0.2, 0.2),
                det(0.9, 0.5, 0.5, 0.9, 0.9),
            ],
            0.6,
        );
        assert_eq!(2, out.len());
    }

    // The O(n^2) escape hatch stops processing.
    #[test]
    #[allow(clippy::cast_precision_loss, clippy::as_conversions)]
    fn test_nms_iteration_cap() {
        // All disjoint -> every retain pass scans everything left.
        let detections = (0..200)
            .map(|i| det(i as f32 / 1000.0, i as f32, 0.0, i as f32 + 0.01, 0.01))
            .collect();
        let out = non_max_suppression(detections, 0.5);
        assert!(out.len() < 200, "{}", out.len());
    }
}
