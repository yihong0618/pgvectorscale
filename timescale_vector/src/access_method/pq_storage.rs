use super::{
    distance::distance_cosine as default_distance,
    graph::{ListSearchNeighbor, ListSearchResult},
    graph_neighbor_store::GraphNeighborStore,
    pg_vector::PgVector,
    plain_node::{ArchivedNode, Node},
    plain_storage::PlainStorageLsnPrivateData,
    pq_quantizer::{PqQuantizer, PqSearchDistanceMeasure, PqVectorElement},
    stats::{
        GreedySearchStats, StatsDistanceComparison, StatsNodeModify, StatsNodeRead, StatsNodeWrite,
        WriteStats,
    },
    storage::{ArchivedData, NodeFullDistanceMeasure, Storage, StorageFullDistanceFromHeap},
    storage_common::{calculate_full_distance, HeapFullDistanceMeasure},
};
use std::{collections::HashMap, iter::once, marker::PhantomData, pin::Pin};

use pgrx::{
    info,
    pg_sys::{AttrNumber, InvalidBlockNumber, InvalidOffsetNumber},
    PgRelation,
};
use rkyv::{vec::ArchivedVec, Archive, Archived, Deserialize, Serialize};

use crate::util::{
    page::PageType, table_slot::TableSlot, tape::Tape, ArchivedItemPointer, HeapPointer,
    IndexPointer, ItemPointer, ReadableBuffer,
};

use super::{meta_page::MetaPage, model::NeighborWithDistance};
use crate::util::WritableBuffer;

pub struct PqCompressionStorage<'a> {
    pub index: &'a PgRelation,
    pub distance_fn: fn(&[f32], &[f32]) -> f32,
    quantizer: PqQuantizer,
    heap_rel: Option<&'a PgRelation>,
    heap_attr: Option<pgrx::pg_sys::AttrNumber>,
}

impl<'a> PqCompressionStorage<'a> {
    pub fn new_for_build(
        index: &'a PgRelation,
        heap_rel: &'a PgRelation,
        heap_attr: pgrx::pg_sys::AttrNumber,
    ) -> PqCompressionStorage<'a> {
        Self {
            index: index,
            distance_fn: default_distance,
            quantizer: PqQuantizer::new(),
            heap_rel: Some(heap_rel),
            heap_attr: Some(heap_attr),
        }
    }

    fn load_quantizer<S: StatsNodeRead>(
        index_relation: &PgRelation,
        meta_page: &super::meta_page::MetaPage,
        stats: &mut S,
    ) -> PqQuantizer {
        unsafe { PqQuantizer::load(&index_relation, meta_page, stats) }
    }

    pub fn load_for_insert<S: StatsNodeRead>(
        heap_rel: &'a PgRelation,
        heap_attr: pgrx::pg_sys::AttrNumber,
        index_relation: &'a PgRelation,
        meta_page: &super::meta_page::MetaPage,
        stats: &mut S,
    ) -> PqCompressionStorage<'a> {
        Self {
            index: index_relation,
            distance_fn: default_distance,
            quantizer: Self::load_quantizer(index_relation, meta_page, stats),
            heap_rel: Some(heap_rel),
            heap_attr: Some(heap_attr),
        }
    }

    pub fn load_for_search(
        index_relation: &'a PgRelation,
        quantizer: &PqQuantizer,
    ) -> PqCompressionStorage<'a> {
        Self {
            index: index_relation,
            distance_fn: default_distance,
            //OPT: get rid of clone
            quantizer: quantizer.clone(),
            heap_rel: None,
            heap_attr: None,
        }
    }

    //todo can we delete?
    fn get_quantized_vector_from_index_pointer<S: StatsNodeRead>(
        &self,
        index_pointer: IndexPointer,
        stats: &mut S,
    ) -> Vec<PqVectorElement> {
        let slot = unsafe { self.get_heap_table_slot_from_index_pointer(index_pointer, stats) };
        let slice = unsafe { slot.get_pg_vector() };
        self.quantizer.quantize(slice.to_slice())
    }

    fn get_quantized_vector_from_heap_pointer<S: StatsNodeRead>(
        &self,
        heap_pointer: HeapPointer,
        stats: &mut S,
    ) -> Vec<PqVectorElement> {
        let slot = unsafe { self.get_heap_table_slot_from_heap_pointer(heap_pointer, stats) };
        let slice = unsafe { slot.get_pg_vector() };
        self.quantizer.quantize(slice.to_slice())
    }

    fn write_quantizer_metadata<S: StatsNodeWrite>(&self, stats: &mut S) {
        let pq = self.quantizer.must_get_pq();
        let index_pointer: IndexPointer = unsafe { super::model::write_pq(pq, &self.index) };
        super::meta_page::MetaPage::update_pq_pointer(&self.index, index_pointer);
    }

    fn visit_lsn_internal(
        &self,
        lsr: &mut ListSearchResult<
            <PqCompressionStorage<'a> as Storage>::QueryDistanceMeasure,
            <PqCompressionStorage<'a> as Storage>::LSNPrivateData,
        >,
        neighbors: &[ItemPointer],
        gns: &GraphNeighborStore,
    ) {
        for (i, &neighbor_index_pointer) in neighbors.iter().enumerate() {
            if !lsr.prepare_insert(neighbor_index_pointer) {
                continue;
            }

            let rn_neighbor =
                unsafe { Node::read(self.index, neighbor_index_pointer, &mut lsr.stats) };
            let node_neighbor = rn_neighbor.get_archived_node();
            let deleted = node_neighbor.is_deleted();

            let distance = match lsr.sdm.as_ref().unwrap() {
                PqSearchDistanceMeasure::Full(query) => {
                    let heap_pointer = node_neighbor.heap_item_pointer.deserialize_item_pointer();
                    if deleted {
                        let pvt_data = PlainStorageLsnPrivateData::new(
                            neighbor_index_pointer,
                            node_neighbor,
                            gns,
                        );
                        self.visit_lsn_internal(lsr, &pvt_data.neighbors, gns);
                        continue;
                        //for deleted nodes, we can't get the distance because we don't know the full vector
                        //so pretend it's the same distance as the parent
                    } else {
                        unsafe {
                            calculate_full_distance(
                                self,
                                heap_pointer,
                                query.to_slice(),
                                &mut lsr.stats,
                            )
                        }
                    }
                }
                PqSearchDistanceMeasure::Pq(table) => {
                    PqSearchDistanceMeasure::calculate_pq_distance(
                        table,
                        node_neighbor.pq_vector.as_slice(),
                        &mut lsr.stats,
                    )
                }
            };
            let lsn = ListSearchNeighbor::new(
                neighbor_index_pointer,
                distance,
                PlainStorageLsnPrivateData::new(neighbor_index_pointer, node_neighbor, gns),
            );

            lsr.insert_neighbor(lsn);
        }
    }
}

impl<'a> Storage for PqCompressionStorage<'a> {
    type QueryDistanceMeasure = PqSearchDistanceMeasure;
    type NodeFullDistanceMeasure<'b> = HeapFullDistanceMeasure<'b, PqCompressionStorage<'b>> where Self: 'b;
    type ArchivedType = ArchivedNode;
    type LSNPrivateData = PlainStorageLsnPrivateData; //no data stored

    fn page_type() -> PageType {
        PageType::Node
    }

    fn create_node<S: StatsNodeWrite>(
        &self,
        full_vector: &[f32],
        heap_pointer: HeapPointer,
        meta_page: &MetaPage,
        tape: &mut Tape,
        stats: &mut S,
    ) -> ItemPointer {
        let pq_vector = self.quantizer.vector_for_new_node(meta_page, full_vector);
        let node = Node::new_for_pq(heap_pointer, pq_vector, meta_page);
        let index_pointer: IndexPointer = node.write(tape, stats);
        index_pointer
    }

    fn start_training(&mut self, meta_page: &super::meta_page::MetaPage) {
        self.quantizer.start_training(meta_page);
    }

    fn add_sample(&mut self, sample: &[f32]) {
        self.quantizer.add_sample(sample);
    }

    fn finish_training(&mut self, stats: &mut WriteStats) {
        self.quantizer.finish_training();
        self.write_quantizer_metadata(stats);
    }

    fn finalize_node_at_end_of_build<S: StatsNodeRead + StatsNodeModify>(
        &mut self,
        meta: &MetaPage,
        index_pointer: IndexPointer,
        neighbors: &Vec<NeighborWithDistance>,
        stats: &mut S,
    ) {
        let node = unsafe { Node::modify(self.index, index_pointer, stats) };
        let mut archived = node.get_archived_node();
        archived.as_mut().set_neighbors(neighbors, &meta);

        let quantized = self.get_quantized_vector_from_heap_pointer(
            archived.heap_item_pointer.deserialize_item_pointer(),
            stats,
        );

        archived.as_mut().set_pq_vector(quantized.as_slice());

        node.commit();
    }

    unsafe fn get_full_vector_distance_state<'b, S: StatsNodeRead>(
        &'b self,
        index_pointer: IndexPointer,
        stats: &mut S,
    ) -> HeapFullDistanceMeasure<'b, PqCompressionStorage<'b>> {
        HeapFullDistanceMeasure::with_index_pointer(self, index_pointer, stats)
    }

    fn get_search_distance_measure(
        &self,
        query: PgVector,
        calc_distance_with_quantizer: bool,
    ) -> PqSearchDistanceMeasure {
        if !calc_distance_with_quantizer {
            return PqSearchDistanceMeasure::Full(query);
        } else {
            return PqSearchDistanceMeasure::Pq(
                self.quantizer
                    .get_distance_table(query.to_slice(), self.distance_fn),
            );
        }
    }

    //todo: same as Bq code?
    fn get_neighbors_with_full_vector_distances_from_disk<
        S: StatsNodeRead + StatsDistanceComparison,
    >(
        &self,
        neighbors_of: ItemPointer,
        result: &mut Vec<NeighborWithDistance>,
        stats: &mut S,
    ) {
        let rn = unsafe { Node::read(self.index, neighbors_of, stats) };
        let heap_pointer = rn
            .get_archived_node()
            .heap_item_pointer
            .deserialize_item_pointer();
        let dist_state =
            unsafe { HeapFullDistanceMeasure::with_heap_pointer(self, heap_pointer, stats) };
        for n in rn.get_archived_node().iter_neighbors() {
            let dist = unsafe { dist_state.get_distance(n, stats) };
            result.push(NeighborWithDistance::new(n, dist))
        }
    }

    /* get_lsn and visit_lsn are different because the distance
    comparisons for BQ get the vector from different places */
    fn create_lsn_for_init_id(
        &self,
        lsr: &mut ListSearchResult<Self::QueryDistanceMeasure, Self::LSNPrivateData>,
        index_pointer: ItemPointer,
        gns: &GraphNeighborStore,
    ) -> ListSearchNeighbor<Self::LSNPrivateData> {
        if !lsr.prepare_insert(index_pointer) {
            panic!("should not have had an init id already inserted");
        }

        let rn = unsafe { Node::read(self.index, index_pointer, &mut lsr.stats) };
        let node = rn.get_archived_node();

        let distance = match lsr.sdm.as_ref().unwrap() {
            PqSearchDistanceMeasure::Full(query) => {
                if node.is_deleted() {
                    //FIXME: need to handle this case
                    panic!("can't handle the case where the init_id node is deleted");
                }
                let heap_pointer = node.heap_item_pointer.deserialize_item_pointer();
                unsafe {
                    calculate_full_distance(self, heap_pointer, query.to_slice(), &mut lsr.stats)
                }
            }
            PqSearchDistanceMeasure::Pq(table) => PqSearchDistanceMeasure::calculate_pq_distance(
                table,
                node.pq_vector.as_slice(),
                &mut lsr.stats,
            ),
        };

        ListSearchNeighbor::new(
            index_pointer,
            distance,
            PlainStorageLsnPrivateData::new(index_pointer, node, gns),
        )
    }

    fn visit_lsn(
        &self,
        lsr: &mut ListSearchResult<Self::QueryDistanceMeasure, Self::LSNPrivateData>,
        lsn_idx: usize,
        gns: &GraphNeighborStore,
    ) {
        let lsn = lsr.get_lsn_by_idx(lsn_idx);
        //clone needed so we don't continue to borrow lsr
        self.visit_lsn_internal(lsr, &lsn.get_private_data().neighbors.clone(), gns);
    }

    fn return_lsn(
        &self,
        lsn: &ListSearchNeighbor<Self::LSNPrivateData>,
        stats: &mut GreedySearchStats,
    ) -> HeapPointer {
        lsn.get_private_data().heap_pointer
    }

    fn set_neighbors_on_disk<S: StatsNodeModify + StatsNodeRead>(
        &self,
        meta: &MetaPage,
        index_pointer: IndexPointer,
        neighbors: &[NeighborWithDistance],
        stats: &mut S,
    ) {
        let node = unsafe { Node::modify(self.index, index_pointer, stats) };
        let mut archived = node.get_archived_node();
        archived.as_mut().set_neighbors(neighbors, &meta);
        node.commit();
    }

    fn get_distance_function(&self) -> fn(&[f32], &[f32]) -> f32 {
        self.distance_fn
    }
}

impl<'a> StorageFullDistanceFromHeap for PqCompressionStorage<'a> {
    unsafe fn get_heap_table_slot_from_index_pointer<S: StatsNodeRead>(
        &self,
        index_pointer: IndexPointer,
        stats: &mut S,
    ) -> TableSlot {
        let rn = unsafe { Node::read(self.index, index_pointer, stats) };
        let node = rn.get_archived_node();
        let heap_pointer = node.heap_item_pointer.deserialize_item_pointer();

        self.get_heap_table_slot_from_heap_pointer(heap_pointer, stats)
    }

    unsafe fn get_heap_table_slot_from_heap_pointer<T: StatsNodeRead>(
        &self,
        heap_pointer: HeapPointer,
        stats: &mut T,
    ) -> TableSlot {
        TableSlot::new(
            self.heap_rel.unwrap(),
            heap_pointer,
            self.heap_attr.unwrap(),
            stats,
        )
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use pgrx::*;

    #[pg_test]
    unsafe fn test_pq_storage_index_creation() -> spi::Result<()> {
        crate::access_method::build::tests::test_index_creation_and_accuracy_scaffold(
            "num_neighbors=38, USE_PQ = TRUE",
        )?;
        Ok(())
    }

    #[pg_test]
    unsafe fn test_pq_storage_index_creation_few_neighbors() -> spi::Result<()> {
        //a test with few neighbors tests the case that nodes share a page, which has caused deadlocks in the past.
        crate::access_method::build::tests::test_index_creation_and_accuracy_scaffold(
            "num_neighbors=10, USE_PQ = TRUE",
        )?;
        Ok(())
    }

    #[test]
    fn test_pq_storage_delete_vacuum_plain() {
        crate::access_method::vacuum::tests::test_delete_vacuum_plain_scaffold(
            "num_neighbors = 10, use_pq = TRUE",
        );
    }

    #[test]
    fn test_pq_storage_delete_vacuum_full() {
        crate::access_method::vacuum::tests::test_delete_vacuum_full_scaffold(
            "num_neighbors = 38, use_pq = TRUE",
        );
    }

    /* can't run test_pq_storage_empty_table_insert because can't create pq index on pq table  */

    #[pg_test]
    unsafe fn test_pq_storage_insert_empty_insert() -> spi::Result<()> {
        let suffix = (1..=253)
            .map(|i| format!("{}", i))
            .collect::<Vec<String>>()
            .join(", ");

        Spi::run(&format!(
            "CREATE TABLE test(embedding vector(256));

            INSERT INTO test (embedding)
            SELECT
                ('[' || i || ',2,3,{suffix}]')::vector
            FROM generate_series(1, 300) i;

            CREATE INDEX idxtest
                  ON test
               USING tsv(embedding)
                WITH (num_neighbors = 10, use_pq = TRUE);

            DELETE FROM test;

            INSERT INTO test(embedding) VALUES ('[1,2,3,{suffix}]'), ('[14,15,16,{suffix}]');
            ",
        ))?;

        let res: Option<i64> = Spi::get_one(&format!(
            "   set enable_seqscan = 0;
                WITH cte as (select * from test order by embedding <=> '[0,0,0,{suffix}]') SELECT count(*) from cte;",
        ))?;
        assert_eq!(2, res.unwrap());

        Spi::run(&format!("drop index idxtest;",))?;

        Ok(())
    }
}