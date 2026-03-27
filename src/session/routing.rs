use std::collections::{HashMap, HashSet};

use crate::control::protocol::{EndpointDirection, EndpointId};

/// Routing table: for each source endpoint, the set of destination endpoints.
/// Automatically rebuilt when endpoints are added/removed.
pub struct RoutingTable {
    routes: HashMap<EndpointId, HashSet<EndpointId>>,
    /// Destinations that receive from 2+ sources and need audio mixing.
    multi_source_dests: HashSet<EndpointId>,
}

impl Default for RoutingTable {
    fn default() -> Self {
        Self::new()
    }
}

impl RoutingTable {
    pub fn new() -> Self {
        Self {
            routes: HashMap::new(),
            multi_source_dests: HashSet::new(),
        }
    }

    /// Rebuild the routing table from the current set of endpoints.
    /// - SendRecv endpoints send to and receive from all other non-SendOnly endpoints
    /// - SendOnly endpoints send to all non-SendOnly endpoints but receive nothing
    /// - RecvOnly endpoints receive from all non-RecvOnly endpoints but send nothing
    /// - Bridge-to-bridge routing is excluded to prevent audio loops
    pub fn rebuild(&mut self, endpoints: &[(EndpointId, EndpointDirection, bool)]) {
        self.routes.clear();

        for &(src_id, src_dir, src_is_bridge) in endpoints {
            // RecvOnly endpoints never produce outgoing media
            if src_dir == EndpointDirection::RecvOnly {
                continue;
            }

            let dests: HashSet<EndpointId> = endpoints
                .iter()
                .filter(|&&(dst_id, dst_dir, dst_is_bridge)| {
                    dst_id != src_id
                        && dst_dir != EndpointDirection::SendOnly
                        && !(src_is_bridge && dst_is_bridge)
                })
                .map(|&(id, _, _)| id)
                .collect();

            if !dests.is_empty() {
                self.routes.insert(src_id, dests);
            }
        }

        // Compute which destinations receive from 2+ sources (need mixing)
        let mut dest_source_count: HashMap<EndpointId, usize> = HashMap::new();
        for dests in self.routes.values() {
            for &d in dests {
                *dest_source_count.entry(d).or_default() += 1;
            }
        }
        self.multi_source_dests = dest_source_count
            .into_iter()
            .filter(|&(_, count)| count >= 2)
            .map(|(id, _)| id)
            .collect();
    }

    /// Get destinations for a given source endpoint
    pub fn destinations(&self, source: &EndpointId) -> Option<&HashSet<EndpointId>> {
        self.routes.get(source)
    }

    /// Returns true if this destination receives from 2+ sources and needs mixing
    #[allow(dead_code)]
    pub fn is_multi_source(&self, dest: &EndpointId) -> bool {
        self.multi_source_dests.contains(dest)
    }

    /// Returns the set of destinations that need mixing
    pub fn multi_source_destinations(&self) -> &HashSet<EndpointId> {
        &self.multi_source_dests
    }

    /// Returns the set of source endpoints that route to a given destination
    pub fn sources_for(&self, dest: &EndpointId) -> HashSet<EndpointId> {
        self.routes
            .iter()
            .filter(|(_, dests)| dests.contains(dest))
            .map(|(&src, _)| src)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn eid() -> EndpointId {
        Uuid::new_v4()
    }

    #[test]
    fn test_two_sendrecv() {
        let a = eid();
        let b = eid();
        let mut rt = RoutingTable::new();
        rt.rebuild(&[
            (a, EndpointDirection::SendRecv, false),
            (b, EndpointDirection::SendRecv, false),
        ]);

        assert!(rt.destinations(&a).unwrap().contains(&b));
        assert!(rt.destinations(&b).unwrap().contains(&a));
    }

    #[test]
    fn test_recvonly_receives_but_doesnt_send() {
        let a = eid();
        let b = eid();
        let c = eid();
        let mut rt = RoutingTable::new();
        rt.rebuild(&[
            (a, EndpointDirection::SendRecv, false),
            (b, EndpointDirection::SendRecv, false),
            (c, EndpointDirection::RecvOnly, false),
        ]);

        // a sends to b and c
        let a_dests = rt.destinations(&a).unwrap();
        assert!(a_dests.contains(&b));
        assert!(a_dests.contains(&c));

        // c doesn't send to anyone
        assert!(rt.destinations(&c).is_none());
    }

    #[test]
    fn test_sendonly_sends_but_doesnt_receive() {
        let a = eid();
        let b = eid();
        let file = eid();
        let mut rt = RoutingTable::new();
        rt.rebuild(&[
            (a, EndpointDirection::SendRecv, false),
            (b, EndpointDirection::SendRecv, false),
            (file, EndpointDirection::SendOnly, false),
        ]);

        // file sends to a and b
        let file_dests = rt.destinations(&file).unwrap();
        assert!(file_dests.contains(&a));
        assert!(file_dests.contains(&b));

        // a sends to b (not file, since file is SendOnly)
        let a_dests = rt.destinations(&a).unwrap();
        assert!(a_dests.contains(&b));
        assert!(!a_dests.contains(&file));
    }

    #[test]
    fn test_endpoint_removal_and_rebuild() {
        let a = eid();
        let b = eid();
        let c = eid();
        let mut rt = RoutingTable::new();

        // Build with 3 sendrecv endpoints
        rt.rebuild(&[
            (a, EndpointDirection::SendRecv, false),
            (b, EndpointDirection::SendRecv, false),
            (c, EndpointDirection::SendRecv, false),
        ]);

        // Verify initial state: each endpoint routes to the other two
        let a_dests = rt.destinations(&a).unwrap();
        assert!(
            a_dests.contains(&b) && a_dests.contains(&c),
            "a should route to b and c"
        );
        let b_dests = rt.destinations(&b).unwrap();
        assert!(
            b_dests.contains(&a) && b_dests.contains(&c),
            "b should route to a and c"
        );
        let c_dests = rt.destinations(&c).unwrap();
        assert!(
            c_dests.contains(&a) && c_dests.contains(&b),
            "c should route to a and b"
        );

        // Remove endpoint 'c' and rebuild
        rt.rebuild(&[
            (a, EndpointDirection::SendRecv, false),
            (b, EndpointDirection::SendRecv, false),
        ]);

        // Verify 'c' is not in any destination list
        let a_dests = rt.destinations(&a).unwrap();
        assert!(a_dests.contains(&b), "a should still route to b");
        assert!(
            !a_dests.contains(&c),
            "a should NOT route to removed endpoint c"
        );

        let b_dests = rt.destinations(&b).unwrap();
        assert!(b_dests.contains(&a), "b should still route to a");
        assert!(
            !b_dests.contains(&c),
            "b should NOT route to removed endpoint c"
        );

        // Removed endpoint should have no routing entry
        assert!(
            rt.destinations(&c).is_none(),
            "removed endpoint c should have no routing destinations"
        );
    }

    #[test]
    fn test_bridge_to_bridge_excluded() {
        let a = eid();
        let bridge1 = eid();
        let bridge2 = eid();
        let mut rt = RoutingTable::new();
        rt.rebuild(&[
            (a, EndpointDirection::SendRecv, false),
            (bridge1, EndpointDirection::SendRecv, true),
            (bridge2, EndpointDirection::SendRecv, true),
        ]);

        // a routes to both bridges
        let a_dests = rt.destinations(&a).unwrap();
        assert!(a_dests.contains(&bridge1));
        assert!(a_dests.contains(&bridge2));

        // bridge1 routes to a but NOT bridge2
        let b1_dests = rt.destinations(&bridge1).unwrap();
        assert!(b1_dests.contains(&a));
        assert!(
            !b1_dests.contains(&bridge2),
            "bridge-to-bridge should be excluded"
        );

        // bridge2 routes to a but NOT bridge1
        let b2_dests = rt.destinations(&bridge2).unwrap();
        assert!(b2_dests.contains(&a));
        assert!(
            !b2_dests.contains(&bridge1),
            "bridge-to-bridge should be excluded"
        );
    }

    #[test]
    fn test_multi_source_dests_three_sendrecv() {
        let a = eid();
        let b = eid();
        let c = eid();
        let mut rt = RoutingTable::new();
        rt.rebuild(&[
            (a, EndpointDirection::SendRecv, false),
            (b, EndpointDirection::SendRecv, false),
            (c, EndpointDirection::SendRecv, false),
        ]);

        // Each endpoint receives from 2 sources → all are multi-source
        assert!(rt.is_multi_source(&a), "a receives from b and c");
        assert!(rt.is_multi_source(&b), "b receives from a and c");
        assert!(rt.is_multi_source(&c), "c receives from a and b");
    }

    #[test]
    fn test_multi_source_dests_two_sendrecv() {
        let a = eid();
        let b = eid();
        let mut rt = RoutingTable::new();
        rt.rebuild(&[
            (a, EndpointDirection::SendRecv, false),
            (b, EndpointDirection::SendRecv, false),
        ]);

        // Each endpoint receives from exactly 1 source → none are multi-source
        assert!(!rt.is_multi_source(&a));
        assert!(!rt.is_multi_source(&b));
    }

    #[test]
    fn test_multi_source_dests_sendonly_plus_two_sendrecv() {
        let a = eid();
        let b = eid();
        let file = eid();
        let mut rt = RoutingTable::new();
        rt.rebuild(&[
            (a, EndpointDirection::SendRecv, false),
            (b, EndpointDirection::SendRecv, false),
            (file, EndpointDirection::SendOnly, false),
        ]);

        // file sends to a and b; a sends to b; b sends to a
        // a receives from b + file = 2 sources → multi-source
        // b receives from a + file = 2 sources → multi-source
        assert!(rt.is_multi_source(&a), "a receives from b and file");
        assert!(rt.is_multi_source(&b), "b receives from a and file");
        assert!(
            !rt.is_multi_source(&file),
            "file is sendonly, receives nothing"
        );
    }

    #[test]
    fn test_multi_source_dests_recvonly_in_three_way() {
        let a = eid();
        let b = eid();
        let c = eid();
        let mut rt = RoutingTable::new();
        rt.rebuild(&[
            (a, EndpointDirection::SendRecv, false),
            (b, EndpointDirection::SendRecv, false),
            (c, EndpointDirection::RecvOnly, false),
        ]);

        // a sends to b and c; b sends to a and c
        // a receives from b only (1 source) → not multi-source
        // c receives from a and b (2 sources) → multi-source
        assert!(!rt.is_multi_source(&a), "a only receives from b");
        assert!(!rt.is_multi_source(&b), "b only receives from a");
        assert!(rt.is_multi_source(&c), "c receives from a and b");
    }

    #[test]
    fn test_sources_for() {
        let a = eid();
        let b = eid();
        let c = eid();
        let mut rt = RoutingTable::new();
        rt.rebuild(&[
            (a, EndpointDirection::SendRecv, false),
            (b, EndpointDirection::SendRecv, false),
            (c, EndpointDirection::SendRecv, false),
        ]);

        let sources_for_a = rt.sources_for(&a);
        assert!(sources_for_a.contains(&b));
        assert!(sources_for_a.contains(&c));
        assert!(!sources_for_a.contains(&a));
        assert_eq!(sources_for_a.len(), 2);
    }
}
