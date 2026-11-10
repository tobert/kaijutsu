//! Where a `kj` result moves a client.

use kaijutsu_types::ContextId;

/// Where a `kj` result run against `current` moves the client. A structured
/// run stays addressed to its context, so a verb that moves the player names
/// the move in its data: `switched_to` (`kj context switch`, `kj fork`), or
/// `rotated_from` with `context_id` when the seat that rotated is `current`.
/// Rotating another seat by label moves nothing. An id that does not parse is
/// an error, never a stay.
pub fn follow_target(data: Option<&serde_json::Value>, current: ContextId) -> Result<Option<ContextId>, String> {
    let Some(data) = data else { return Ok(None) };
    let id_at = |key: &str| -> Result<Option<ContextId>, String> {
        match data.get(key) {
            None => Ok(None),
            Some(value) => value
                .as_str()
                .ok_or_else(|| format!("`{key}` is not a string"))
                .and_then(|hex| ContextId::parse(hex).map_err(|e| format!("`{key}` {hex:?}: {e}")))
                .map(Some),
        }
    };
    if let Some(target) = id_at("switched_to")? {
        return Ok((target != current).then_some(target));
    }
    if id_at("rotated_from")? == Some(current) {
        return id_at("context_id")?
            .map(Some)
            .ok_or_else(|| "rotate result names no successor".to_string());
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rotate_data(from: ContextId, successor: ContextId) -> serde_json::Value {
        serde_json::json!({"context_id": successor.to_hex(), "rotated_from": from.to_hex()})
    }

    /// Rotating the seat on screen follows its successor; rotating another
    /// seat by label leaves the view where it is.
    #[test]
    fn a_rotate_result_follows_only_the_seat_it_ran_against() {
        let (from, next, other) = (ContextId::new(), ContextId::new(), ContextId::new());
        assert_eq!(follow_target(Some(&rotate_data(from, next)), from), Ok(Some(next)));
        assert_eq!(follow_target(Some(&rotate_data(other, next)), from), Ok(None));
        let no_successor = serde_json::json!({"rotated_from": from.to_hex()});
        assert!(follow_target(Some(&no_successor), from).is_err());
    }

    #[test]
    fn a_switch_result_follows_its_target() {
        let (here, there) = (ContextId::new(), ContextId::new());
        let switched = serde_json::json!({"switched_to": there.to_hex()});
        assert_eq!(follow_target(Some(&switched), here), Ok(Some(there)));
    }

    /// A record that merely mentions a context id (`kj context info`) moves
    /// nothing, and an id that does not parse is an error, never a stay.
    #[test]
    fn other_results_stay_and_a_bad_id_is_an_error() {
        let here = ContextId::new();
        let info = serde_json::json!({"context_id": ContextId::new().to_hex(), "label": "x"});
        assert_eq!(follow_target(Some(&info), here), Ok(None));
        assert_eq!(follow_target(None, here), Ok(None));
        let bad = serde_json::json!({"switched_to": "not-an-id"});
        assert!(follow_target(Some(&bad), here).is_err());
        let not_a_string = serde_json::json!({"switched_to": 7});
        assert!(follow_target(Some(&not_a_string), here).is_err());
    }
}
