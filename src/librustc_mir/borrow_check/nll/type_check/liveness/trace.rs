use crate::borrow_check::location::LocationTable;
use crate::borrow_check::nll::region_infer::values::{self, PointIndex, RegionValueElements};
use crate::borrow_check::nll::type_check::liveness::local_use_map::LocalUseMap;
use crate::borrow_check::nll::type_check::NormalizeLocation;
use crate::borrow_check::nll::type_check::TypeChecker;
use crate::dataflow::indexes::MovePathIndex;
use crate::dataflow::move_paths::MoveData;
use crate::dataflow::{FlowAtLocation, FlowsAtLocation, MaybeInitializedPlaces};
use rustc::infer::canonical::QueryRegionConstraint;
use rustc::mir::{BasicBlock, ConstraintCategory, Local, Location, Mir};
use rustc::traits::query::dropck_outlives::DropckOutlivesResult;
use rustc::traits::query::type_op::outlives::DropckOutlives;
use rustc::traits::query::type_op::TypeOp;
use rustc::ty::{Ty, TypeFoldable};
use rustc_data_structures::bit_set::HybridBitSet;
use rustc_data_structures::fx::FxHashMap;
use std::rc::Rc;

/// This is the heart of the liveness computation. For each variable X
/// that requires a liveness computation, it walks over all the uses
/// of X and does a reverse depth-first search ("trace") through the
/// MIR. This search stops when we find a definition of that variable.
/// The points visited in this search is the USE-LIVE set for the variable;
/// of those points is added to all the regions that appear in the variable's
/// type.
///
/// We then also walks through each *drop* of those variables and does
/// another search, stopping when we reach a use or definition. This
/// is the DROP-LIVE set of points. Each of the points in the
/// DROP-LIVE set are to the liveness sets for regions found in the
/// `dropck_outlives` result of the variable's type (in particular,
/// this respects `#[may_dangle]` annotations).
pub(super) fn trace(
    typeck: &mut TypeChecker<'_, 'gcx, 'tcx>,
    mir: &Mir<'tcx>,
    elements: &Rc<RegionValueElements>,
    flow_inits: &mut FlowAtLocation<'tcx, MaybeInitializedPlaces<'_, 'gcx, 'tcx>>,
    move_data: &MoveData<'tcx>,
    live_locals: Vec<Local>,
    location_table: &LocationTable,
) {
    debug!("trace()");

    let local_use_map = &LocalUseMap::build(&live_locals, elements, mir);

    let cx = LivenessContext {
        typeck,
        mir,
        flow_inits,
        elements,
        local_use_map,
        move_data,
        drop_data: FxHashMap::default(),
        location_table,
    };

    LivenessResults::new(cx).compute_for_all_locals(live_locals);
}

/// Contextual state for the type-liveness generator.
struct LivenessContext<'me, 'typeck, 'flow, 'gcx, 'tcx>
where
    'typeck: 'me,
    'flow: 'me,
    'tcx: 'typeck + 'flow,
    'gcx: 'tcx,
{
    /// Current type-checker, giving us our inference context etc.
    typeck: &'me mut TypeChecker<'typeck, 'gcx, 'tcx>,

    /// Defines the `PointIndex` mapping
    elements: &'me RegionValueElements,

    /// MIR we are analyzing.
    mir: &'me Mir<'tcx>,

    /// Mapping to/from the various indices used for initialization tracking.
    move_data: &'me MoveData<'tcx>,

    /// Cache for the results of `dropck_outlives` query.
    drop_data: FxHashMap<Ty<'tcx>, DropData<'tcx>>,

    /// Results of dataflow tracking which variables (and paths) have been
    /// initialized.
    flow_inits: &'me mut FlowAtLocation<'tcx, MaybeInitializedPlaces<'flow, 'gcx, 'tcx>>,

    /// Index indicating where each variable is assigned, used, or
    /// dropped.
    local_use_map: &'me LocalUseMap,

    /// Maps between a MIR Location and a LocationIndex
    location_table: &'me LocationTable,
}

struct DropData<'tcx> {
    dropck_result: DropckOutlivesResult<'tcx>,
    region_constraint_data: Option<Rc<Vec<QueryRegionConstraint<'tcx>>>>,
}

struct LivenessResults<'me, 'typeck, 'flow, 'gcx, 'tcx>
where
    'typeck: 'me,
    'flow: 'me,
    'tcx: 'typeck + 'flow,
    'gcx: 'tcx,
{
    cx: LivenessContext<'me, 'typeck, 'flow, 'gcx, 'tcx>,

    /// Set of points that define the current local.
    defs: HybridBitSet<PointIndex>,

    /// Points where the current variable is "use live" -- meaning
    /// that there is a future "full use" that may use its value.
    use_live_at: HybridBitSet<PointIndex>,

    /// Points where the current variable is "drop live" -- meaning
    /// that there is no future "full use" that may use its value, but
    /// there is a future drop.
    drop_live_at: HybridBitSet<PointIndex>,

    /// Locations where drops may occur.
    drop_locations: Vec<Location>,

    /// Stack used when doing (reverse) DFS.
    stack: Vec<PointIndex>,
}

impl LivenessResults<'me, 'typeck, 'flow, 'gcx, 'tcx> {
    fn new(cx: LivenessContext<'me, 'typeck, 'flow, 'gcx, 'tcx>) -> Self {
        let num_points = cx.elements.num_points();
        LivenessResults {
            cx,
            defs: HybridBitSet::new_empty(num_points),
            use_live_at: HybridBitSet::new_empty(num_points),
            drop_live_at: HybridBitSet::new_empty(num_points),
            drop_locations: vec![],
            stack: vec![],
        }
    }

    fn compute_for_all_locals(&mut self, live_locals: Vec<Local>) {
        for local in live_locals {
            self.reset_local_state();
            self.add_defs_for(local);
            self.compute_use_live_points_for(local);
            self.compute_drop_live_points_for(local);

            let local_ty = self.cx.mir.local_decls[local].ty;

            if !self.use_live_at.is_empty() {
                self.cx.add_use_live_facts_for(local_ty, &self.use_live_at);
            }

            if !self.drop_live_at.is_empty() {
                self.cx.add_drop_live_facts_for(
                    local,
                    local_ty,
                    &self.drop_locations,
                    &self.drop_live_at,
                );
            }
        }
    }

    /// Clear the value of fields that are "per local variable".
    fn reset_local_state(&mut self) {
        self.defs.clear();
        self.use_live_at.clear();
        self.drop_live_at.clear();
        self.drop_locations.clear();
        assert!(self.stack.is_empty());
    }

    /// Adds the definitions of `local` into `self.defs`.
    fn add_defs_for(&mut self, local: Local) {
        for def in self.cx.local_use_map.defs(local) {
            debug!("- defined at {:?}", def);
            self.defs.insert(def);
        }
    }

    /// Computes all points where local is "use live" -- meaning its
    /// current value may be used later (except by a drop). This is
    /// done by walking backwards from each use of `local` until we
    /// find a `def` of local.
    ///
    /// Requires `add_defs_for(local)` to have been executed.
    fn compute_use_live_points_for(&mut self, local: Local) {
        debug!("compute_use_live_points_for(local={:?})", local);

        self.stack.extend(self.cx.local_use_map.uses(local));
        while let Some(p) = self.stack.pop() {
            if self.defs.contains(p) {
                continue;
            }

            if self.use_live_at.insert(p) {
                self.cx
                    .elements
                    .push_predecessors(self.cx.mir, p, &mut self.stack)
            }
        }
    }

    /// Computes all points where local is "drop live" -- meaning its
    /// current value may be dropped later (but not used). This is
    /// done by iterating over the drops of `local` where `local` (or
    /// some subpart of `local`) is initialized. For each such drop,
    /// we walk backwards until we find a point where `local` is
    /// either defined or use-live.
    ///
    /// Requires `compute_use_live_points_for` and `add_defs_for` to
    /// have been executed.
    fn compute_drop_live_points_for(&mut self, local: Local) {
        debug!("compute_drop_live_points_for(local={:?})", local);

        let mpi = self.cx.move_data.rev_lookup.find_local(local);
        debug!("compute_drop_live_points_for: mpi = {:?}", mpi);

        // Find the drops where `local` is initialized.
        for drop_point in self.cx.local_use_map.drops(local) {
            let location = self.cx.elements.to_location(drop_point);
            debug_assert_eq!(self.cx.mir.terminator_loc(location.block), location,);

            if self.cx.initialized_at_terminator(location.block, mpi) {
                if self.drop_live_at.insert(drop_point) {
                    self.drop_locations.push(location);
                    self.stack.push(drop_point);
                }
            }
        }

        debug!(
            "compute_drop_live_points_for: drop_locations={:?}",
            self.drop_locations
        );

        // Reverse DFS. But for drops, we do it a bit differently.
        // The stack only ever stores *terminators of blocks*. Within
        // a block, we walk back the statements in an inner loop.
        'next_block: while let Some(term_point) = self.stack.pop() {
            self.compute_drop_live_points_for_block(mpi, term_point);
        }
    }

    /// Executes one iteration of the drop-live analysis loop.
    ///
    /// The parameter `mpi` is the `MovePathIndex` of the local variable
    /// we are currently analyzing.
    ///
    /// The point `term_point` represents some terminator in the MIR,
    /// where the local `mpi` is drop-live on entry to that terminator.
    ///
    /// This method adds all drop-live points within the block and --
    /// where applicable -- pushes the terminators of preceding blocks
    /// onto `self.stack`.
    fn compute_drop_live_points_for_block(&mut self, mpi: MovePathIndex, term_point: PointIndex) {
        debug!(
            "compute_drop_live_points_for_block(mpi={:?}, term_point={:?})",
            self.cx.move_data.move_paths[mpi].place,
            self.cx.elements.to_location(term_point),
        );

        // We are only invoked with terminators where `mpi` is
        // drop-live on entry.
        debug_assert!(self.drop_live_at.contains(term_point));

        // Otherwise, scan backwards through the statements in the
        // block.  One of them may be either a definition or use
        // live point.
        let term_location = self.cx.elements.to_location(term_point);
        debug_assert_eq!(
            self.cx.mir.terminator_loc(term_location.block),
            term_location,
        );
        let block = term_location.block;
        let entry_point = self.cx.elements.entry_point(term_location.block);
        for p in (entry_point..term_point).rev() {
            debug!(
                "compute_drop_live_points_for_block: p = {:?}",
                self.cx.elements.to_location(p),
            );

            if self.defs.contains(p) {
                debug!("compute_drop_live_points_for_block: def site");
                return;
            }

            if self.use_live_at.contains(p) {
                debug!("compute_drop_live_points_for_block: use-live at {:?}", p);
                return;
            }

            if !self.drop_live_at.insert(p) {
                debug!("compute_drop_live_points_for_block: already drop-live");
                return;
            }
        }

        for &pred_block in self.cx.mir.predecessors_for(block).iter() {
            debug!(
                "compute_drop_live_points_for_block: pred_block = {:?}",
                pred_block,
            );

            // Check whether the variable is (at least partially)
            // initialized at the exit of this predecessor. If so, we
            // want to enqueue it on our list. If not, go check the
            // next block.
            //
            // Note that we only need to check whether `live_local`
            // became de-initialized at basic block boundaries. If it
            // were to become de-initialized within the block, that
            // would have been a "use-live" transition in the earlier
            // loop, and we'd have returned already.
            //
            // NB. It's possible that the pred-block ends in a call
            // which stores to the variable; in that case, the
            // variable may be uninitialized "at exit" because this
            // call only considers the *unconditional effects* of the
            // terminator. *But*, in that case, the terminator is also
            // a *definition* of the variable, in which case we want
            // to stop the search anyhow. (But see Note 1 below.)
            if !self.cx.initialized_at_exit(pred_block, mpi) {
                debug!("compute_drop_live_points_for_block: not initialized");
                continue;
            }

            let pred_term_loc = self.cx.mir.terminator_loc(pred_block);
            let pred_term_point = self.cx.elements.point_from_location(pred_term_loc);

            // If the terminator of this predecessor either *assigns*
            // our value or is a "normal use", then stop.
            if self.defs.contains(pred_term_point) {
                debug!(
                    "compute_drop_live_points_for_block: defined at {:?}",
                    pred_term_loc
                );
                continue;
            }

            if self.use_live_at.contains(pred_term_point) {
                debug!(
                    "compute_drop_live_points_for_block: use-live at {:?}",
                    pred_term_loc
                );
                continue;
            }

            // Otherwise, we are drop-live on entry to the terminator,
            // so walk it.
            if self.drop_live_at.insert(pred_term_point) {
                debug!("compute_drop_live_points_for_block: pushed to stack");
                self.stack.push(pred_term_point);
            }
        }

        // Note 1. There is a weird scenario that you might imagine
        // being problematic here, but which actually cannot happen.
        // The problem would be if we had a variable that *is* initialized
        // (but dead) on entry to the terminator, and where the current value
        // will be dropped in the case of unwind. In that case, we ought to
        // consider `X` to be drop-live in between the last use and call.
        // Here is the example:
        //
        // ```
        // BB0 {
        //   X = ...
        //   use(X); // last use
        //   ...     // <-- X ought to be drop-live here
        //   X = call() goto BB1 unwind BB2
        // }
        //
        // BB1 {
        //   DROP(X)
        // }
        //
        // BB2 {
        //   DROP(X)
        // }
        // ```
        //
        // However, the current code would, when walking back from BB2,
        // simply stop and never explore BB0. This seems bad! But it turns
        // out this code is flawed anyway -- note that the existing value of
        // `X` would leak in the case where unwinding did *not* occur.
        //
        // What we *actually* generate is a store to a temporary
        // for the call (`TMP = call()...`) and then a
        // `DropAndReplace` to swap that with `X`
        // (`DropAndReplace` has very particular semantics).
    }
}

impl LivenessContext<'_, '_, '_, '_, 'tcx> {
    /// Returns `true` if the local variable (or some part of it) is initialized in
    /// the terminator of `block`. We need to check this to determine if a
    /// DROP of some local variable will have an effect -- note that
    /// drops, as they may unwind, are always terminators.
    fn initialized_at_terminator(&mut self, block: BasicBlock, mpi: MovePathIndex) -> bool {
        // Compute the set of initialized paths at terminator of block
        // by resetting to the start of the block and then applying
        // the effects of all statements. This is the only way to get
        // "just ahead" of a terminator.
        self.flow_inits.reset_to_entry_of(block);
        for statement_index in 0..self.mir[block].statements.len() {
            let location = Location {
                block,
                statement_index,
            };
            self.flow_inits.reconstruct_statement_effect(location);
            self.flow_inits.apply_local_effect(location);
        }

        self.flow_inits.has_any_child_of(mpi).is_some()
    }

    /// Returns `true` if the path `mpi` (or some part of it) is initialized at
    /// the exit of `block`.
    ///
    /// **Warning:** Does not account for the result of `Call`
    /// instructions.
    fn initialized_at_exit(&mut self, block: BasicBlock, mpi: MovePathIndex) -> bool {
        self.flow_inits.reset_to_exit_of(block);
        self.flow_inits.has_any_child_of(mpi).is_some()
    }

    /// Stores the result that all regions in `value` are live for the
    /// points `live_at`.
    fn add_use_live_facts_for(
        &mut self,
        value: impl TypeFoldable<'tcx>,
        live_at: &HybridBitSet<PointIndex>,
    ) {
        debug!("add_use_live_facts_for(value={:?})", value);

        Self::make_all_regions_live(
            self.elements,
            &mut self.typeck,
            value,
            live_at,
            self.location_table,
        )
    }

    /// Some variable with type `live_ty` is "drop live" at `location`
    /// -- i.e., it may be dropped later. This means that *some* of
    /// the regions in its type must be live at `location`. The
    /// precise set will depend on the dropck constraints, and in
    /// particular this takes `#[may_dangle]` into account.
    fn add_drop_live_facts_for(
        &mut self,
        dropped_local: Local,
        dropped_ty: Ty<'tcx>,
        drop_locations: &[Location],
        live_at: &HybridBitSet<PointIndex>,
    ) {
        debug!(
            "add_drop_live_constraint(\
             dropped_local={:?}, \
             dropped_ty={:?}, \
             drop_locations={:?}, \
             live_at={:?})",
            dropped_local,
            dropped_ty,
            drop_locations,
            values::location_set_str(self.elements, live_at.iter()),
        );

        let drop_data = self.drop_data.entry(dropped_ty).or_insert_with({
            let typeck = &mut self.typeck;
            move || Self::compute_drop_data(typeck, dropped_ty)
        });

        if let Some(data) = &drop_data.region_constraint_data {
            for &drop_location in drop_locations {
                self.typeck
                    .push_region_constraints(
                        drop_location.to_locations(),
                        ConstraintCategory::Boring,
                        data,
                    );
            }
        }

        drop_data.dropck_result.report_overflows(
            self.typeck.infcx.tcx,
            self.mir.source_info(*drop_locations.first().unwrap()).span,
            dropped_ty,
        );

        // All things in the `outlives` array may be touched by
        // the destructor and must be live at this point.
        for &kind in &drop_data.dropck_result.kinds {
            Self::make_all_regions_live(
                self.elements,
                &mut self.typeck,
                kind,
                live_at,
                self.location_table,
            );
        }
    }

    fn make_all_regions_live(
        elements: &RegionValueElements,
        typeck: &mut TypeChecker<'_, '_, 'tcx>,
        value: impl TypeFoldable<'tcx>,
        live_at: &HybridBitSet<PointIndex>,
        location_table: &LocationTable,
    ) {
        debug!("make_all_regions_live(value={:?})", value);
        debug!(
            "make_all_regions_live: live_at={}",
            values::location_set_str(elements, live_at.iter()),
        );

        let tcx = typeck.tcx();
        tcx.for_each_free_region(&value, |live_region| {
            let live_region_vid = typeck.borrowck_context
                .universal_regions
                .to_region_vid(live_region);
            typeck.borrowck_context
                .constraints
                .liveness_constraints
                .add_elements(live_region_vid, live_at);

            if let Some(facts) = typeck.borrowck_context.all_facts {
                for point in live_at.iter() {
                    let loc = elements.to_location(point);
                    facts.region_live_at.push((live_region_vid, location_table.start_index(loc)));
                    facts.region_live_at.push((live_region_vid, location_table.mid_index(loc)));
                }
            }
        });
    }

    fn compute_drop_data(
        typeck: &mut TypeChecker<'_, 'gcx, 'tcx>,
        dropped_ty: Ty<'tcx>,
    ) -> DropData<'tcx> {
        debug!("compute_drop_data(dropped_ty={:?})", dropped_ty,);

        let param_env = typeck.param_env;
        let (dropck_result, region_constraint_data) = param_env
            .and(DropckOutlives::new(dropped_ty))
            .fully_perform(typeck.infcx)
            .unwrap();

        DropData {
            dropck_result,
            region_constraint_data,
        }
    }
}
