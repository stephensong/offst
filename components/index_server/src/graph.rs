use std::{cmp, hash};
use std::collections::HashMap;

use crate::bfs::bfs;

/*
use proto::conn::BoxFuture;

trait Graph {
    type Request;
    type Mutation;

    fn request(&mut self, request: Self::Request) -> BoxFuture<'_, Self::Response>;
    fn mutate(&mut self, mutation: Self::Mutation) -> BoxFuture<'_, ()>;
}
*/

type CapacityEdge = (u128, u128);

struct CapacityGraph<N> {
    nodes: HashMap<N,HashMap<N,CapacityEdge>>,
}


impl<N> CapacityGraph<N> 
where
    N: cmp::Eq + hash::Hash + Clone,
{

    pub fn new() -> CapacityGraph<N> {
        CapacityGraph {
            nodes: HashMap::new(),
        }
    }

    /// Add or update edge
    pub fn update_edge(&mut self, a: N, b: N, edge: CapacityEdge) -> Option<CapacityEdge> {
        let mut a_entry = self.nodes.entry(a).or_insert(HashMap::new());
        a_entry.insert(b, edge)
    }

    /// Remove an edge from the graph
    pub fn remove_edge(&mut self, a: &N, b: &N) -> Option<CapacityEdge> {
        let mut a_map = match self.nodes.get_mut(a) {
            Some(a_map) => a_map,
            None => return None,
        };

        let old_edge = match a_map.remove(b) {
            Some(edge) => edge,
            None => return None,
        };

        if a_map.len() == 0 {
            self.nodes.remove(a);
        }

        Some(old_edge)
    }

    /// Remove a node and all related edges known from him.
    /// Note: This method will not remove an edge from another node b pointing to a.
    pub fn remove_node(&mut self, a: &N) -> Option<HashMap<N, CapacityEdge>> {
        self.nodes.remove(a)
    } 

    /// Get a directed edge (if exists) 
    fn get_edge(&self, a: &N, b: &N) -> Option<CapacityEdge> {
        match self.nodes.get(a) {
            None => None,
            Some(a_map) => {
                match a_map.get(b) {
                    None => None,
                    Some(a_b_edge) => Some(*a_b_edge),
                }
            }
        }
    }

    /// Get the send capacity from `a` to a direct neighbor `b`.
    /// This is calculated as the minimum send capacity reported by `a` and the maximum recv
    /// capacity reported by `b`.
    fn get_send_capacity(&self, a: &N, b: &N) -> u128 {
        let a_b_edge = if let Some(a_b_edge) = self.get_edge(&a,&b) {
            a_b_edge 
        } else {
            return 0;
        };

        let b_a_edge = if let Some(b_a_edge) = self.get_edge(&b,&a) {
            b_a_edge 
        } else {
            return 0;
        };

        let (a_send, _a_recv) = a_b_edge;
        let (_b_send, b_recv) = b_a_edge;

        cmp::min(a_send, b_recv)
    }

    fn neighbors_with_send_capacity(&self, a: N, capacity: u128) -> Option<impl Iterator<Item=&N>> {
        let a_map = self.nodes.get(&a)?;
        let iter = a_map.keys().filter(move |b| self.get_send_capacity(&a,b) >= capacity);
        Some(iter)
    }

    /// Calculate the amount of capacity we can send through a route.
    /// This amount if the minimum of all edge capacities of the route.
    fn get_route_capacity(&self, route: &[N]) -> Option<u128> {
        (0 .. route.len().checked_sub(1)?)
            .map(|i| self.get_send_capacity(&route[i], &route[i+1]))
            .min()
    }

    /// Get a route with capacity at least `capacity`. 
    /// Returns the route together with the capacity it is possible to send through the route.
    pub fn get_route(&self, a: &N, b: &N, capacity: u128) -> Option<(Vec<N>, u128)> {
        let get_neighbors = |b: &N| self.neighbors_with_send_capacity(b.clone(), capacity).unwrap();
        let route = bfs(a, b, get_neighbors)?;
        // We assert that we will always have valid capacity here:
        let capacity = self.get_route_capacity(&route).unwrap();

        Some((route, capacity))
    }

    /// A loop from myself through given neighbor, back to myself.
    /// a -> neighbor -> ... -> ... -> a
    pub fn get_loop_from(&self, a: &N, neighbor: &N, capacity: u128) -> Option<(Vec<N>, u128)> {
        let get_neighbors = move |cur_node: &N| {
            // The following directed edge is not allowed: (neighbor -> a)
            let cur_node_is_neighbor = cur_node == neighbor;
            self.neighbors_with_send_capacity(cur_node.clone(), capacity)
                .unwrap()
                .filter(move |&next_node| !cur_node_is_neighbor || (next_node != a))
        };

        let route = bfs(a, a, get_neighbors)?;
        // We expect that bfs does not return the trivial route (a)
        assert!(route.len() >= 2);

        // We assert that we will always have valid capacity here:
        let capacity = self.get_route_capacity(&route).unwrap();
        Some((route, capacity))
    }

    /// A loop from myself back to myself through given neighbor.
    /// a -> ... -> ... -> neighbor -> a
    pub fn get_loop_to(&self, a: &N, neighbor: &N, capacity: u128) -> Option<(Vec<N>, u128)> {
        let get_neighbors = move |cur_node: &N| {
            // The following directed edge is not allowed: (a -> neighbor)
            let cur_node_is_a = cur_node == a;
            self.neighbors_with_send_capacity(cur_node.clone(), capacity)
                .unwrap()
                .filter(move |&next_node| !cur_node_is_a || (next_node != neighbor))
        };

        let route = bfs(a, a, get_neighbors)?;
        // We expect that bfs does not return the trivial route (a)
        assert!(route.len() >= 2);

        // We assert that we will always have valid capacity here:
        let capacity = self.get_route_capacity(&route).unwrap();
        Some((route, capacity))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_send_capacity_basic() {
        let mut cg = CapacityGraph::<u32>::new();
        cg.update_edge(0, 1, (10, 20));
        cg.update_edge(1, 0, (15, 5));

        assert_eq!(cg.get_send_capacity(&0, &1), cmp::min(5, 10));
        assert_eq!(cg.get_send_capacity(&1, &0), cmp::min(15, 20));
    }

    #[test]
    fn test_get_send_capacity_one_sided() {
        let mut cg = CapacityGraph::<u32>::new();
        cg.update_edge(0, 1, (10, 20));

        assert_eq!(cg.get_send_capacity(&0, &1), 0);
        assert_eq!(cg.get_send_capacity(&1, &0), 0);
    }

    #[test]
    fn test_add_remove_edge() {
        let mut cg = CapacityGraph::<u32>::new();
        assert_eq!(cg.remove_edge(&0, &1), None);
        cg.update_edge(0, 1, (10, 20));
        assert_eq!(cg.nodes.len(), 1);

        assert_eq!(cg.remove_edge(&0, &1), Some((10,20)));
        assert_eq!(cg.nodes.len(), 0);

        cg.update_edge(0, 1, (10,20));
        assert_eq!(cg.nodes.len(), 1);
        cg.remove_node(&1);
        assert_eq!(cg.nodes.len(), 1);
    }

    #[test]
    fn test_get_route() {
        /*
         * Example graph:
         *
         *    0 --> 1 --> 2 --> 5
         *          |     ^
         *          V     |
         *          3 --> 4
         *
        */

        let mut cg = CapacityGraph::<u32>::new();

        cg.update_edge(0, 1, (30, 10));
        cg.update_edge(1, 0, (10, 30));

        cg.update_edge(1, 2, (10, 10));
        cg.update_edge(2, 1, (10, 10));

        cg.update_edge(2, 5, (30, 5));
        cg.update_edge(2, 5, (5, 30));

        cg.update_edge(1, 3, (30, 8));
        cg.update_edge(3, 1, (8, 30));

        cg.update_edge(3, 4, (30, 6));
        cg.update_edge(4, 3, (6, 30));

        cg.update_edge(4, 2, (30, 18));
        cg.update_edge(2, 4, (18, 30));

        assert_eq!(cg.get_route(&0, &5, 25), Some((vec![0,1,3,4,2,5], 30)));
    }
}

