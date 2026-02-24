//! Density-based clustering of context embeddings via HDBSCAN.

use petal_clustering::Fit;

use crate::IndexError;

/// Compute clusters from embeddings using HDBSCAN.
///
/// Input: `(slot, embedding)` pairs.
/// Output: `(cluster_id, [slot_ids])` pairs. Noise points are excluded.
pub fn compute_clusters(
    embeddings: &[(u32, Vec<f32>)],
    min_cluster_size: usize,
) -> Result<Vec<(usize, Vec<u32>)>, IndexError> {
    if embeddings.len() < min_cluster_size {
        return Ok(vec![]);
    }

    let dims = embeddings[0].1.len();
    let n = embeddings.len();

    // Build ndarray matrix (n x dims), f64 for petal-clustering
    let mut flat: Vec<f64> = Vec::with_capacity(n * dims);
    for (_, emb) in embeddings {
        for &v in emb {
            flat.push(v as f64);
        }
    }

    let matrix = ndarray::Array2::from_shape_vec((n, dims), flat)
        .map_err(|e| IndexError::Index(format!("matrix shape: {}", e)))?;

    // Run HDBSCAN
    // fit() returns (clusters: HashMap<usize, Vec<usize>>, outliers: Vec<usize>, scores: Vec<f64>)
    let mut clusterer = petal_clustering::HDbscan {
        min_cluster_size,
        min_samples: min_cluster_size,
        ..Default::default()
    };

    let (cluster_map, _outliers, _scores) = clusterer.fit(&matrix, None);

    // Map point indices back to slot IDs
    let mut result: Vec<(usize, Vec<u32>)> = cluster_map
        .into_iter()
        .map(|(cluster_id, point_indices)| {
            let slots: Vec<u32> = point_indices
                .into_iter()
                .map(|idx| embeddings[idx].0)
                .collect();
            (cluster_id, slots)
        })
        .collect();

    result.sort_by_key(|(id, _)| *id);

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_too_few_points() {
        let embeddings = vec![(0u32, vec![1.0, 0.0])];
        let result = compute_clusters(&embeddings, 3).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_distinct_clusters() {
        // 3 tight clusters of 5 points each in 2D
        let mut embeddings = Vec::new();
        let mut slot = 0u32;

        // Cluster A: around (0, 0)
        for _ in 0..5 {
            embeddings.push((slot, vec![0.0 + (slot as f32 * 0.01), 0.0]));
            slot += 1;
        }
        // Cluster B: around (10, 0)
        for _ in 0..5 {
            embeddings.push((slot, vec![10.0 + (slot as f32 * 0.01), 0.0]));
            slot += 1;
        }
        // Cluster C: around (0, 10)
        for _ in 0..5 {
            embeddings.push((slot, vec![0.0, 10.0 + (slot as f32 * 0.01)]));
            slot += 1;
        }

        let result = compute_clusters(&embeddings, 3).unwrap();
        // HDBSCAN should find at least 2 clusters (exact count depends on density)
        assert!(
            result.len() >= 2,
            "expected at least 2 clusters, got {}",
            result.len()
        );
    }
}
