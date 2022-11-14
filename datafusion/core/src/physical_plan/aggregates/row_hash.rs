// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! Hash aggregation through row format

use std::sync::Arc;
use std::task::{Context, Poll};
use std::vec;

use ahash::RandomState;
use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::stream::{Stream, StreamExt};

use crate::error::Result;
use crate::execution::context::TaskContext;
use crate::execution::memory_manager::ConsumerType;
use crate::execution::{MemoryConsumer, MemoryConsumerId, MemoryManager};
use crate::physical_plan::aggregates::{
    evaluate_group_by, evaluate_many, group_schema, AccumulatorItemV2, AggregateMode,
    PhysicalGroupBy,
};
use crate::physical_plan::hash_utils::create_row_hashes;
use crate::physical_plan::metrics::{BaselineMetrics, RecordOutput};
use crate::physical_plan::{aggregates, AggregateExpr, PhysicalExpr};
use crate::physical_plan::{RecordBatchStream, SendableRecordBatchStream};

use arrow::compute::cast;
use arrow::datatypes::Schema;
use arrow::{array::ArrayRef, compute};
use arrow::{
    array::{Array, UInt32Builder},
    error::{ArrowError, Result as ArrowResult},
};
use arrow::{datatypes::SchemaRef, record_batch::RecordBatch};
use datafusion_common::{DataFusionError, ScalarValue};
use datafusion_row::accessor::RowAccessor;
use datafusion_row::layout::RowLayout;
use datafusion_row::reader::{read_row, RowReader};
use datafusion_row::writer::{write_row, RowWriter};
use datafusion_row::{MutableRecordBatch, RowType};
use hashbrown::raw::RawTable;

/// Grouping aggregate with row-format aggregation states inside.
///
/// For each aggregation entry, we use:
/// - [Compact] row represents grouping keys for fast hash computation and comparison directly on raw bytes.
/// - [WordAligned] row to store aggregation state, designed to be CPU-friendly when updates over every field are often.
///
/// The architecture is the following:
///
/// 1. For each input RecordBatch, update aggregation states corresponding to all appeared grouping keys.
/// 2. At the end of the aggregation (e.g. end of batches in a partition), the accumulator converts its state to a RecordBatch of a single row
/// 3. The RecordBatches of all accumulators are merged (`concatenate` in `rust/arrow`) together to a single RecordBatch.
/// 4. The state's RecordBatch is `merge`d to a new state
/// 5. The state is mapped to the final value
///
/// [Compact]: datafusion_row::layout::RowType::Compact
/// [WordAligned]: datafusion_row::layout::RowType::WordAligned
pub(crate) struct GroupedHashAggregateStreamV2 {
    stream: BoxStream<'static, ArrowResult<RecordBatch>>,
    schema: SchemaRef,
}

/// Actual implementation of [`GroupedHashAggregateStreamV2`].
///
/// This is wrapped into yet another struct because we need to interact with the async memory management subsystem
/// during poll. To have as little code "weirdness" as possible, we chose to just use [`BoxStream`] together with
/// [`futures::stream::unfold`]. The latter requires a state object, which is [`GroupedHashAggregateStreamV2Inner`].
struct GroupedHashAggregateStreamV2Inner {
    schema: SchemaRef,
    input: SendableRecordBatchStream,
    mode: AggregateMode,
    aggr_state: AggregationState,
    aggregate_expressions: Vec<Vec<Arc<dyn PhysicalExpr>>>,

    group_by: PhysicalGroupBy,
    accumulators: Vec<AccumulatorItemV2>,

    group_schema: SchemaRef,
    aggr_schema: SchemaRef,
    aggr_layout: Arc<RowLayout>,

    baseline_metrics: BaselineMetrics,
    random_state: RandomState,
    /// size to be used for resulting RecordBatches
    batch_size: usize,
    /// if the result is chunked into batches,
    /// last offset is preserved for continuation.
    row_group_skip_position: usize,
}

fn aggr_state_schema(aggr_expr: &[Arc<dyn AggregateExpr>]) -> Result<SchemaRef> {
    let fields = aggr_expr
        .iter()
        .flat_map(|expr| expr.state_fields().unwrap().into_iter())
        .collect::<Vec<_>>();
    Ok(Arc::new(Schema::new(fields)))
}

impl GroupedHashAggregateStreamV2 {
    /// Create a new GroupedRowHashAggregateStream
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        mode: AggregateMode,
        schema: SchemaRef,
        group_by: PhysicalGroupBy,
        aggr_expr: Vec<Arc<dyn AggregateExpr>>,
        input: SendableRecordBatchStream,
        baseline_metrics: BaselineMetrics,
        batch_size: usize,
        context: Arc<TaskContext>,
        partition: usize,
    ) -> Result<Self> {
        let timer = baseline_metrics.elapsed_compute().timer();

        // The expressions to evaluate the batch, one vec of expressions per aggregation.
        // Assume create_schema() always put group columns in front of aggr columns, we set
        // col_idx_base to group expression count.
        let aggregate_expressions =
            aggregates::aggregate_expressions(&aggr_expr, &mode, group_by.expr.len())?;

        let accumulators = aggregates::create_accumulators_v2(&aggr_expr)?;

        let group_schema = group_schema(&schema, group_by.expr.len());
        let aggr_schema = aggr_state_schema(&aggr_expr)?;

        let aggr_layout = Arc::new(RowLayout::new(&aggr_schema, RowType::WordAligned));

        let aggr_state = AggregationState {
            memory_consumer: AggregationStateMemoryConsumer {
                id: MemoryConsumerId::new(partition),
                memory_manager: Arc::clone(&context.runtime_env().memory_manager),
                used: 0,
            },
            map: RawTable::with_capacity(0),
            group_states: Vec::with_capacity(0),
        };
        context
            .runtime_env()
            .register_requester(aggr_state.memory_consumer.id());

        timer.done();

        let inner = GroupedHashAggregateStreamV2Inner {
            schema: Arc::clone(&schema),
            mode,
            input,
            group_by,
            accumulators,
            group_schema,
            aggr_schema,
            aggr_layout,
            baseline_metrics,
            aggregate_expressions,
            aggr_state,
            random_state: Default::default(),
            batch_size,
            row_group_skip_position: 0,
        };

        let stream = futures::stream::unfold(inner, |mut this| async move {
            let elapsed_compute = this.baseline_metrics.elapsed_compute();

            loop {
                let result: ArrowResult<Option<RecordBatch>> =
                    match this.input.next().await {
                        Some(Ok(batch)) => {
                            let timer = elapsed_compute.timer();
                            let result = group_aggregate_batch(
                                &this.mode,
                                &this.random_state,
                                &this.group_by,
                                &mut this.accumulators,
                                &this.group_schema,
                                this.aggr_layout.clone(),
                                batch,
                                &mut this.aggr_state,
                                &this.aggregate_expressions,
                            )
                            .await;

                            timer.done();

                            match result {
                                Ok(_) => continue,
                                Err(e) => Err(ArrowError::ExternalError(Box::new(e))),
                            }
                        }
                        Some(Err(e)) => Err(e),
                        None => {
                            let timer = this.baseline_metrics.elapsed_compute().timer();
                            let result = create_batch_from_map(
                                &this.mode,
                                &this.group_schema,
                                &this.aggr_schema,
                                this.batch_size,
                                this.row_group_skip_position,
                                &mut this.aggr_state,
                                &mut this.accumulators,
                                &this.schema,
                            );

                            timer.done();
                            result
                        }
                    };

                this.row_group_skip_position += this.batch_size;
                match result {
                    Ok(Some(result)) => {
                        return Some((
                            Ok(result.record_output(&this.baseline_metrics)),
                            this,
                        ));
                    }
                    Ok(None) => return None,
                    Err(error) => return Some((Err(error), this)),
                }
            }
        });

        // seems like some consumers call this stream even after it returned `None`, so let's fuse the stream.
        let stream = stream.fuse();
        let stream = Box::pin(stream);

        Ok(Self { schema, stream })
    }
}

impl Stream for GroupedHashAggregateStreamV2 {
    type Item = ArrowResult<RecordBatch>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        this.stream.poll_next_unpin(cx)
    }
}

impl RecordBatchStream for GroupedHashAggregateStreamV2 {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
}

/// TODO: Make this a member function of [`GroupedHashAggregateStreamV2`]
#[allow(clippy::too_many_arguments)]
async fn group_aggregate_batch(
    mode: &AggregateMode,
    random_state: &RandomState,
    grouping_set: &PhysicalGroupBy,
    accumulators: &mut [AccumulatorItemV2],
    group_schema: &Schema,
    state_layout: Arc<RowLayout>,
    batch: RecordBatch,
    aggr_state: &mut AggregationState,
    aggregate_expressions: &[Vec<Arc<dyn PhysicalExpr>>],
) -> Result<()> {
    // evaluate the grouping expressions
    let grouping_by_values = evaluate_group_by(grouping_set, &batch)?;

    let AggregationState {
        map,
        group_states,
        memory_consumer,
    } = aggr_state;
    let mut memory_pool = ShortLivedMemoryPool::new(memory_consumer);

    for group_values in grouping_by_values {
        let group_rows: Vec<Vec<u8>> = create_group_rows(group_values, group_schema);

        // evaluate the aggregation expressions.
        // We could evaluate them after the `take`, but since we need to evaluate all
        // of them anyways, it is more performant to do it while they are together.
        let aggr_input_values = evaluate_many(aggregate_expressions, &batch)?;

        // 1.1 construct the key from the group values
        // 1.2 construct the mapping key if it does not exist
        // 1.3 add the row' index to `indices`

        // track which entries in `aggr_state` have rows in this batch to aggregate
        let mut groups_with_rows = vec![];

        // 1.1 Calculate the group keys for the group values
        let mut batch_hashes = vec![0; batch.num_rows()];
        create_row_hashes(&group_rows, random_state, &mut batch_hashes)?;

        for (row, hash) in batch_hashes.into_iter().enumerate() {
            let entry = map.get_mut(hash, |(_hash, group_idx)| {
                // verify that a group that we are inserting with hash is
                // actually the same key value as the group in
                // existing_idx  (aka group_values @ row)
                let group_state = &group_states[*group_idx];
                group_rows[row] == group_state.group_by_values
            });

            match entry {
                // Existing entry for this group value
                Some((_hash, group_idx)) => {
                    let group_state = &mut group_states[*group_idx];

                    // 1.3
                    if group_state.indices.is_empty() {
                        groups_with_rows.push(*group_idx);
                    };

                    // ensure we have enough indices allocated
                    if group_state.indices.capacity() == group_state.indices.len() {
                        // allocate more

                        // growth factor: 2, but at least 2 elements
                        let bump_elements = (group_state.indices.capacity() * 2).max(2);
                        let bump_size = std::mem::size_of::<u32>() * bump_elements;

                        memory_pool.alloc(bump_size).await?;

                        group_state.indices.reserve(bump_elements);
                    }

                    group_state.indices.push(row as u32); // remember this row
                }
                //  1.2 Need to create new entry
                None => {
                    // Add new entry to group_states and save newly created index
                    let group_state = RowGroupState {
                        group_by_values: group_rows[row].clone(),
                        aggregation_buffer: vec![0; state_layout.fixed_part_width()],
                        indices: vec![row as u32], // 1.3
                    };
                    let group_idx = group_states.len();

                    // NOTE: do NOT include the `RowGroupState` struct size in here because this is captured by
                    // `group_states` (see allocation check down below)
                    let mut bump_size_total = (std::mem::size_of::<u8>()
                        * group_state.group_by_values.capacity())
                        + (std::mem::size_of::<u8>()
                            * group_state.aggregation_buffer.capacity())
                        + (std::mem::size_of::<u32>() * group_state.indices.capacity());

                    // ensure that `group_states` has enough space
                    let reserve_groups_states =
                        if group_states.capacity() == group_states.len() {
                            // growth factor: 2, but at least 16 elements
                            let bump_elements = (group_states.capacity() * 2).max(16);
                            let bump_size =
                                bump_elements * std::mem::size_of::<RowGroupState>();
                            bump_size_total += bump_size;

                            Some(bump_elements)
                        } else {
                            None
                        };

                    // for hasher function, use precomputed hash value
                    let reserve_map =
                        if map.try_insert_no_grow(hash, (hash, group_idx)).is_err() {
                            // need to request more memory

                            let bump_elements = (map.capacity() * 2).max(16);
                            let bump_size =
                                bump_elements * std::mem::size_of::<(u64, usize)>();
                            bump_size_total += bump_size;

                            Some(bump_elements)
                        } else {
                            None
                        };

                    // allocate once
                    memory_pool.alloc(bump_size_total).await?;

                    if let Some(bump_elements) = reserve_groups_states {
                        group_states.reserve(bump_elements);
                    }
                    group_states.push(group_state);

                    groups_with_rows.push(group_idx);

                    if let Some(bump_elements) = reserve_map {
                        map.reserve(bump_elements, |(hash, _group_index)| *hash);

                        // still need to insert the element since first try failed
                        map.try_insert_no_grow(hash, (hash, group_idx))
                            .expect("just grew the container");
                    }
                }
            };
        }

        // Collect all indices + offsets based on keys in this vec
        let mut batch_indices: UInt32Builder = UInt32Builder::with_capacity(0);
        let mut offsets = vec![0];
        let mut offset_so_far = 0;
        for group_idx in groups_with_rows.iter() {
            let indices = &group_states[*group_idx].indices;
            batch_indices.append_slice(indices);
            offset_so_far += indices.len();
            offsets.push(offset_so_far);
        }
        let batch_indices = batch_indices.finish();

        // `Take` all values based on indices into Arrays
        let values: Vec<Vec<Arc<dyn Array>>> = aggr_input_values
            .iter()
            .map(|array| {
                array
                    .iter()
                    .map(|array| {
                        compute::take(
                            array.as_ref(),
                            &batch_indices,
                            None, // None: no index check
                        )
                        .unwrap()
                    })
                    .collect()
                // 2.3
            })
            .collect();

        // 2.1 for each key in this batch
        // 2.2 for each aggregation
        // 2.3 `slice` from each of its arrays the keys' values
        // 2.4 update / merge the accumulator with the values
        // 2.5 clear indices
        groups_with_rows
            .iter()
            .zip(offsets.windows(2))
            .try_for_each(|(group_idx, offsets)| {
                let group_state = &mut group_states[*group_idx];
                // 2.2
                accumulators
                    .iter_mut()
                    .zip(values.iter())
                    .map(|(accumulator, aggr_array)| {
                        (
                            accumulator,
                            aggr_array
                                .iter()
                                .map(|array| {
                                    // 2.3
                                    array.slice(offsets[0], offsets[1] - offsets[0])
                                })
                                .collect::<Vec<ArrayRef>>(),
                        )
                    })
                    .try_for_each(|(accumulator, values)| {
                        let mut state_accessor =
                            RowAccessor::new_from_layout(state_layout.clone());
                        state_accessor
                            .point_to(0, group_state.aggregation_buffer.as_mut_slice());
                        match mode {
                            AggregateMode::Partial => {
                                accumulator.update_batch(&values, &mut state_accessor)
                            }
                            AggregateMode::FinalPartitioned | AggregateMode::Final => {
                                // note: the aggregation here is over states, not values, thus the merge
                                accumulator.merge_batch(&values, &mut state_accessor)
                            }
                        }
                    })
                    // 2.5
                    .and({
                        group_state.indices.clear();
                        Ok(())
                    })
            })?;
    }

    Ok(())
}

/// The state that is built for each output group.
#[derive(Debug)]
struct RowGroupState {
    /// The actual group by values, stored sequentially
    group_by_values: Vec<u8>,

    // Accumulator state, stored sequentially
    aggregation_buffer: Vec<u8>,

    /// scratch space used to collect indices for input rows in a
    /// bach that have values to aggregate. Reset on each batch
    indices: Vec<u32>,
}

/// The state of all the groups
struct AggregationState {
    memory_consumer: AggregationStateMemoryConsumer,

    /// Logically maps group values to an index in `group_states`
    ///
    /// Uses the raw API of hashbrown to avoid actually storing the
    /// keys in the table
    ///
    /// keys: u64 hashes of the GroupValue
    /// values: (hash, index into `group_states`)
    map: RawTable<(u64, usize)>,

    /// State for each group
    group_states: Vec<RowGroupState>,
}

impl std::fmt::Debug for AggregationState {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        // hashes are not store inline, so could only get values
        let map_string = "RawTable";
        f.debug_struct("AggregationState")
            .field("map", &map_string)
            .field("group_states", &self.group_states)
            .finish()
    }
}

/// Accounting data structure for memory usage.
struct AggregationStateMemoryConsumer {
    /// Consumer ID.
    id: MemoryConsumerId,

    /// Linked memory manager.
    memory_manager: Arc<MemoryManager>,

    /// Currently used size in bytes.
    used: usize,
}

#[async_trait]
impl MemoryConsumer for AggregationStateMemoryConsumer {
    fn name(&self) -> String {
        "AggregationState".to_owned()
    }

    fn id(&self) -> &crate::execution::MemoryConsumerId {
        &self.id
    }

    fn memory_manager(&self) -> Arc<MemoryManager> {
        Arc::clone(&self.memory_manager)
    }

    fn type_(&self) -> &ConsumerType {
        &ConsumerType::Tracking
    }

    async fn spill(&self) -> Result<usize> {
        Err(DataFusionError::ResourcesExhausted(
            "Cannot spill AggregationState".to_owned(),
        ))
    }

    fn mem_used(&self) -> usize {
        self.used
    }
}

impl Drop for AggregationStateMemoryConsumer {
    fn drop(&mut self) {
        self.memory_manager
            .drop_consumer(self.id(), self.mem_used());
    }
}

/// Memory pool that can be used in a function scope.
///
/// This is helpful if there are many small memory allocations (so the overhead if tracking them in [`MemoryManager`] is
/// high due to lock contention) and pre-calculating the entire allocation for a whole [`RecordBatch`] is complicated or
/// expensive.
///
/// The pool will try to allocate a whole block of memory and gives back overallocated memory on [drop](Self::drop).
struct ShortLivedMemoryPool<'a> {
    pool: &'a mut AggregationStateMemoryConsumer,
    block_size: usize,
    remaining: usize,
}

impl<'a> ShortLivedMemoryPool<'a> {
    fn new(pool: &'a mut AggregationStateMemoryConsumer) -> Self {
        Self {
            pool,
            block_size: 1024 * 1024, // 1MB
            remaining: 0,
        }
    }

    async fn alloc(&mut self, mut bytes: usize) -> Result<()> {
        // are there enough bytes left within the current block?
        if bytes <= self.remaining {
            self.remaining -= bytes;
            return Ok(());
        }

        // we can already use the remaining bytes from the current block
        bytes -= self.remaining;

        // need to allocate a new block
        let alloc_size = bytes.max(self.block_size);
        self.pool.try_grow(alloc_size).await?;
        self.pool.used += alloc_size;
        self.remaining = alloc_size - bytes;

        Ok(())
    }
}

impl<'a> Drop for ShortLivedMemoryPool<'a> {
    fn drop(&mut self) {
        // give back over-allocated memory
        self.pool.shrink(self.remaining);
        self.pool.used -= self.remaining;
    }
}

/// Create grouping rows
fn create_group_rows(arrays: Vec<ArrayRef>, schema: &Schema) -> Vec<Vec<u8>> {
    let mut writer = RowWriter::new(schema, RowType::Compact);
    let mut results = vec![];
    for cur_row in 0..arrays[0].len() {
        write_row(&mut writer, cur_row, schema, &arrays);
        results.push(writer.get_row().to_vec());
        writer.reset()
    }
    results
}

/// Create a RecordBatch with all group keys and accumulator' states or values.
#[allow(clippy::too_many_arguments)]
fn create_batch_from_map(
    mode: &AggregateMode,
    group_schema: &Schema,
    aggr_schema: &Schema,
    batch_size: usize,
    skip_items: usize,
    aggr_state: &mut AggregationState,
    accumulators: &mut [AccumulatorItemV2],
    output_schema: &Schema,
) -> ArrowResult<Option<RecordBatch>> {
    if skip_items > aggr_state.group_states.len() {
        return Ok(None);
    }

    if aggr_state.group_states.is_empty() {
        return Ok(Some(RecordBatch::new_empty(Arc::new(
            output_schema.to_owned(),
        ))));
    }

    let mut state_accessor = RowAccessor::new(aggr_schema, RowType::WordAligned);

    let (group_buffers, mut state_buffers): (Vec<_>, Vec<_>) = aggr_state
        .group_states
        .iter()
        .skip(skip_items)
        .take(batch_size)
        .map(|gs| (gs.group_by_values.clone(), gs.aggregation_buffer.clone()))
        .unzip();

    let mut columns: Vec<ArrayRef> =
        read_as_batch(&group_buffers, group_schema, RowType::Compact);

    match mode {
        AggregateMode::Partial => columns.extend(read_as_batch(
            &state_buffers,
            aggr_schema,
            RowType::WordAligned,
        )),
        AggregateMode::Final | AggregateMode::FinalPartitioned => {
            let mut results: Vec<Vec<ScalarValue>> = vec![vec![]; accumulators.len()];
            for buffer in state_buffers.iter_mut() {
                state_accessor.point_to(0, buffer);
                for (i, acc) in accumulators.iter().enumerate() {
                    results[i].push(acc.evaluate(&state_accessor).unwrap());
                }
            }
            for scalars in results {
                columns.push(ScalarValue::iter_to_array(scalars)?);
            }
        }
    }

    // cast output if needed (e.g. for types like Dictionary where
    // the intermediate GroupByScalar type was not the same as the
    // output
    let columns = columns
        .iter()
        .zip(output_schema.fields().iter())
        .map(|(col, desired_field)| cast(col, desired_field.data_type()))
        .collect::<ArrowResult<Vec<_>>>()?;

    RecordBatch::try_new(Arc::new(output_schema.to_owned()), columns).map(Some)
}

fn read_as_batch(rows: &[Vec<u8>], schema: &Schema, row_type: RowType) -> Vec<ArrayRef> {
    let row_num = rows.len();
    let mut output = MutableRecordBatch::new(row_num, Arc::new(schema.clone()));
    let mut row = RowReader::new(schema, row_type);

    for data in rows {
        row.point_to(0, data);
        read_row(&row, &mut output, schema);
    }

    output.output_as_columns()
}
