use crate::utils::{
    has_drop, in_macro_or_desugar, is_copy, match_def_path, match_type, paths, snippet_opt, span_lint_hir,
    span_lint_hir_and_then, walk_ptrs_ty_depth,
};
use if_chain::if_chain;
use matches::matches;
use rustc::hir::intravisit::FnKind;
use rustc::hir::{def_id, Body, FnDecl, HirId};
use rustc::lint::{LateContext, LateLintPass, LintArray, LintPass};
use rustc::mir::{
    self, traversal,
    visit::{MutatingUseContext, PlaceContext, Visitor},
    TerminatorKind,
};
use rustc::ty::{self, Ty};
use rustc::{declare_lint_pass, declare_tool_lint};
use rustc_errors::Applicability;
use std::convert::TryFrom;
use syntax::source_map::{BytePos, Span};

macro_rules! unwrap_or_continue {
    ($x:expr) => {
        match $x {
            Some(x) => x,
            None => continue,
        }
    };
}

declare_clippy_lint! {
    /// **What it does:** Checks for a redudant `clone()` (and its relatives) which clones an owned
    /// value that is going to be dropped without further use.
    ///
    /// **Why is this bad?** It is not always possible for the compiler to eliminate useless
    /// allocations and deallocations generated by redundant `clone()`s.
    ///
    /// **Known problems:**
    ///
    /// * Suggestions made by this lint could require NLL to be enabled.
    /// * False-positive if there is a borrow preventing the value from moving out.
    ///
    /// ```rust
    /// let x = String::new();
    ///
    /// let y = &x;
    ///
    /// foo(x.clone()); // This lint suggests to remove this `clone()`
    /// ```
    ///
    /// **Example:**
    /// ```rust
    /// {
    ///     let x = Foo::new();
    ///     call(x.clone());
    ///     call(x.clone()); // this can just pass `x`
    /// }
    ///
    /// ["lorem", "ipsum"].join(" ").to_string()
    ///
    /// Path::new("/a/b").join("c").to_path_buf()
    /// ```
    pub REDUNDANT_CLONE,
    nursery,
    "`clone()` of an owned value that is going to be dropped immediately"
}

declare_lint_pass!(RedundantClone => [REDUNDANT_CLONE]);

impl<'a, 'tcx> LateLintPass<'a, 'tcx> for RedundantClone {
    #[allow(clippy::too_many_lines)]
    fn check_fn(
        &mut self,
        cx: &LateContext<'a, 'tcx>,
        _: FnKind<'tcx>,
        _: &'tcx FnDecl,
        body: &'tcx Body,
        _: Span,
        _: HirId,
    ) {
        let def_id = cx.tcx.hir().body_owner_def_id(body.id());
        let mir = cx.tcx.optimized_mir(def_id);

        for (bb, bbdata) in mir.basic_blocks().iter_enumerated() {
            let terminator = bbdata.terminator();

            if in_macro_or_desugar(terminator.source_info.span) {
                continue;
            }

            // Give up on loops
            if terminator.successors().any(|s| *s == bb) {
                continue;
            }

            let (fn_def_id, arg, arg_ty, _) = unwrap_or_continue!(is_call_with_ref_arg(cx, mir, &terminator.kind));

            let from_borrow = match_def_path(cx, fn_def_id, &paths::CLONE_TRAIT_METHOD)
                || match_def_path(cx, fn_def_id, &paths::TO_OWNED_METHOD)
                || (match_def_path(cx, fn_def_id, &paths::TO_STRING_METHOD) && match_type(cx, arg_ty, &paths::STRING));

            let from_deref = !from_borrow
                && (match_def_path(cx, fn_def_id, &paths::PATH_TO_PATH_BUF)
                    || match_def_path(cx, fn_def_id, &paths::OS_STR_TO_OS_STRING));

            if !from_borrow && !from_deref {
                continue;
            }

            // _1 in MIR `{ _2 = &_1; clone(move _2); }` or `{ _2 = _1; to_path_buf(_2); } (from_deref)
            // In case of `from_deref`, `arg` is already a reference since it is `deref`ed in the previous
            // block.
            let (cloned, cannot_move_out) = unwrap_or_continue!(find_stmt_assigns_to(
                cx,
                mir,
                arg,
                from_borrow,
                bbdata.statements.iter()
            ));

            if from_borrow && cannot_move_out {
                continue;
            }

            // _1 in MIR `{ _2 = &_1; _3 = deref(move _2); } -> { _4 = _3; to_path_buf(move _4); }`
            let referent = if from_deref {
                let ps = mir.predecessors_for(bb);
                if ps.len() != 1 {
                    continue;
                }
                let pred_terminator = mir[ps[0]].terminator();

                let pred_arg = if_chain! {
                    if let Some((pred_fn_def_id, pred_arg, pred_arg_ty, Some(res))) =
                        is_call_with_ref_arg(cx, mir, &pred_terminator.kind);
                    if *res == mir::Place::Base(mir::PlaceBase::Local(cloned));
                    if match_def_path(cx, pred_fn_def_id, &paths::DEREF_TRAIT_METHOD);
                    if match_type(cx, pred_arg_ty, &paths::PATH_BUF)
                        || match_type(cx, pred_arg_ty, &paths::OS_STRING);
                    then {
                        pred_arg
                    } else {
                        continue;
                    }
                };

                let (local, cannot_move_out) = unwrap_or_continue!(find_stmt_assigns_to(
                    cx,
                    mir,
                    pred_arg,
                    true,
                    mir[ps[0]].statements.iter()
                ));
                if cannot_move_out {
                    continue;
                }
                local
            } else {
                cloned
            };

            let used_later = traversal::ReversePostorder::new(&mir, bb).skip(1).any(|(tbb, tdata)| {
                // Give up on loops
                if tdata.terminator().successors().any(|s| *s == bb) {
                    return true;
                }

                let mut vis = LocalUseVisitor {
                    local: referent,
                    used_other_than_drop: false,
                };
                vis.visit_basic_block_data(tbb, tdata);
                vis.used_other_than_drop
            });

            if !used_later {
                let span = terminator.source_info.span;
                let node = if let mir::ClearCrossCrate::Set(scope_local_data) = &mir.source_scope_local_data {
                    scope_local_data[terminator.source_info.scope].lint_root
                } else {
                    unreachable!()
                };

                if_chain! {
                    if let Some(snip) = snippet_opt(cx, span);
                    if let Some(dot) = snip.rfind('.');
                    then {
                        let sugg_span = span.with_lo(
                            span.lo() + BytePos(u32::try_from(dot).unwrap())
                        );

                        span_lint_hir_and_then(cx, REDUNDANT_CLONE, node, sugg_span, "redundant clone", |db| {
                            db.span_suggestion(
                                sugg_span,
                                "remove this",
                                String::new(),
                                Applicability::MaybeIncorrect,
                            );
                            db.span_note(
                                span.with_hi(span.lo() + BytePos(u32::try_from(dot).unwrap())),
                                "this value is dropped without further use",
                            );
                        });
                    } else {
                        span_lint_hir(cx, REDUNDANT_CLONE, node, span, "redundant clone");
                    }
                }
            }
        }
    }
}

/// If `kind` is `y = func(x: &T)` where `T: !Copy`, returns `(DefId of func, x, T, y)`.
fn is_call_with_ref_arg<'tcx>(
    cx: &LateContext<'_, 'tcx>,
    mir: &'tcx mir::Mir<'tcx>,
    kind: &'tcx mir::TerminatorKind<'tcx>,
) -> Option<(def_id::DefId, mir::Local, Ty<'tcx>, Option<&'tcx mir::Place<'tcx>>)> {
    if_chain! {
        if let TerminatorKind::Call { func, args, destination, .. } = kind;
        if args.len() == 1;
        if let mir::Operand::Move(mir::Place::Base(mir::PlaceBase::Local(local))) = &args[0];
        if let ty::FnDef(def_id, _) = func.ty(&*mir, cx.tcx).sty;
        if let (inner_ty, 1) = walk_ptrs_ty_depth(args[0].ty(&*mir, cx.tcx));
        if !is_copy(cx, inner_ty);
        then {
            Some((def_id, *local, inner_ty, destination.as_ref().map(|(dest, _)| dest)))
        } else {
            None
        }
    }
}

type CannotMoveOut = bool;

/// Finds the first `to = (&)from`, and returns
/// ``Some((from, [`true` if `from` cannot be moved out]))``.
fn find_stmt_assigns_to<'a, 'tcx: 'a>(
    cx: &LateContext<'_, 'tcx>,
    mir: &mir::Mir<'tcx>,
    to: mir::Local,
    by_ref: bool,
    stmts: impl DoubleEndedIterator<Item = &'a mir::Statement<'tcx>>,
) -> Option<(mir::Local, CannotMoveOut)> {
    stmts
        .rev()
        .find_map(|stmt| {
            if let mir::StatementKind::Assign(mir::Place::Base(mir::PlaceBase::Local(local)), v) = &stmt.kind {
                if *local == to {
                    return Some(v);
                }
            }

            None
        })
        .and_then(|v| {
            if by_ref {
                if let mir::Rvalue::Ref(_, _, ref place) = **v {
                    return base_local_and_movability(cx, mir, place);
                }
            } else if let mir::Rvalue::Use(mir::Operand::Copy(ref place)) = **v {
                return base_local_and_movability(cx, mir, place);
            }
            None
        })
}

/// Extracts and returns the undermost base `Local` of given `place`. Returns `place` itself
/// if it is already a `Local`.
///
/// Also reports whether given `place` cannot be moved out.
fn base_local_and_movability<'tcx>(
    cx: &LateContext<'_, 'tcx>,
    mir: &mir::Mir<'tcx>,
    mut place: &mir::Place<'tcx>,
) -> Option<(mir::Local, CannotMoveOut)> {
    use rustc::mir::Place::*;
    use rustc::mir::PlaceBase;

    // Dereference. You cannot move things out from a borrowed value.
    let mut deref = false;
    // Accessing a field of an ADT that has `Drop`. Moving the field out will cause E0509.
    let mut field = false;

    loop {
        match place {
            Base(PlaceBase::Local(local)) => return Some((*local, deref || field)),
            Projection(proj) => {
                place = &proj.base;
                deref = deref || matches!(proj.elem, mir::ProjectionElem::Deref);
                if !field && matches!(proj.elem, mir::ProjectionElem::Field(..)) {
                    field = has_drop(cx, place.ty(&mir.local_decls, cx.tcx).ty);
                }
            },
            _ => return None,
        }
    }
}

struct LocalUseVisitor {
    local: mir::Local,
    used_other_than_drop: bool,
}

impl<'tcx> mir::visit::Visitor<'tcx> for LocalUseVisitor {
    fn visit_basic_block_data(&mut self, block: mir::BasicBlock, data: &mir::BasicBlockData<'tcx>) {
        let statements = &data.statements;
        for (statement_index, statement) in statements.iter().enumerate() {
            self.visit_statement(statement, mir::Location { block, statement_index });

            // Once flagged, skip remaining statements
            if self.used_other_than_drop {
                return;
            }
        }

        self.visit_terminator(
            data.terminator(),
            mir::Location {
                block,
                statement_index: statements.len(),
            },
        );
    }

    fn visit_local(&mut self, local: &mir::Local, ctx: PlaceContext, _: mir::Location) {
        match ctx {
            PlaceContext::MutatingUse(MutatingUseContext::Drop) | PlaceContext::NonUse(_) => return,
            _ => {},
        }

        if *local == self.local {
            self.used_other_than_drop = true;
        }
    }
}
