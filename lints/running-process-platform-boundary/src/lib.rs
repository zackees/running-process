#![feature(rustc_private)]

extern crate rustc_hir;
extern crate rustc_span;

use dylint_linting::declare_late_lint;
use rustc_hir::{def::Res, Expr, ExprKind, QPath, Ty, TyKind};
use rustc_lint::{LateContext, LateLintPass, LintContext};

const BASELINE: &str = include_str!("../../../platform_compliance_baseline.toml");
const RAW_APIS: [&str; 3] = [
    "tokio::process::Command",
    "tokio::process::Child",
    "std::process::Stdio",
];

declare_late_lint! {
    /// ### What it does
    ///
    /// Rejects a raw process API recorded in `raw_platform_apis.toml` outside
    /// `running-process-platform-internal`, except for an exact, ratcheted
    /// Phase-0 baseline entry.
    ///
    /// ### Why is this bad?
    ///
    /// The async migration has one platform choke point. Direct Tokio process
    /// handles and stdio policy bypass typed containment, cleanup, and future
    /// actor ownership rules.
    ///
    /// ### Example
    ///
    /// ```rust,ignore
    /// let _ = std::process::Stdio::piped();
    /// ```
    ///
    /// Use a manifest-listed `running_process_platform_internal` capability
    /// instead.
    pub RP_RAW_PLATFORM_API_OUTSIDE_INTERNAL,
    Deny,
    "raw platform process APIs are restricted to running-process-platform-internal"
}

impl<'tcx> LateLintPass<'tcx> for RpRawPlatformApiOutsideInternal {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &Expr<'tcx>) {
        let ExprKind::Path(ref qpath) = expr.kind else {
            return;
        };
        self.check_raw_path(cx, qpath, expr.hir_id, expr.span);
    }

    fn check_ty(&mut self, cx: &LateContext<'tcx>, ty: &'tcx Ty<'tcx, rustc_hir::AmbigArg>) {
        let TyKind::Path(ref qpath) = ty.kind else {
            return;
        };
        self.check_raw_path(cx, qpath, ty.hir_id, ty.span);
    }
}

impl RpRawPlatformApiOutsideInternal {
    fn check_raw_path<'tcx>(
        &self,
        cx: &LateContext<'tcx>,
        qpath: &QPath<'tcx>,
        hir_id: rustc_hir::HirId,
        span: rustc_span::Span,
    ) {
        let Res::Def(_, def_id) = cx.qpath_res(qpath, hir_id) else {
            return;
        };
        let definition = cx.tcx.def_path_str(def_id);
        let Some(raw_api) = RAW_APIS.iter().copied().find(|api| {
            definition == *api
                || definition
                    .strip_prefix(api)
                    .is_some_and(|suffix| suffix.starts_with("::"))
        }) else {
            return;
        };

        let filename = cx.sess().source_map().span_to_filename(span);
        let source = format!("{filename:?}").replace('\\', "/");
        if source.contains("crates/running-process-platform-internal/")
            || baseline_allows(raw_api, &source)
        {
            return;
        }

        cx.tcx.dcx().span_err(
            span,
            format!(
                "forbidden raw platform API `{raw_api}`; use a blessed running_process_platform_internal capability listed in blessed_api.toml"
            ),
        );
    }
}

fn baseline_allows(raw_api: &str, source: &str) -> bool {
    let mut entry_api: Option<&str> = None;
    let mut entry_path: Option<&str> = None;

    for line in BASELINE.lines().chain(std::iter::once("[[exception]]")) {
        let line = line.trim();
        if line == "[[exception]]" {
            if entry_api == Some(raw_api) && entry_path.is_some_and(|path| source.contains(path)) {
                return true;
            }
            entry_api = None;
            entry_path = None;
            continue;
        }
        if let Some(value) = line.strip_prefix("symbol = \"") {
            entry_api = value.strip_suffix('"');
        }
        if let Some(value) = line.strip_prefix("path = \"") {
            entry_path = value.strip_suffix('"');
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::baseline_allows;

    #[test]
    fn baseline_requires_exact_symbol_and_path() {
        assert!(baseline_allows(
            "tokio::process::Command",
            "/workspace/crates/running-process/src/spawn.rs"
        ));
        assert!(!baseline_allows(
            "tokio::process::Command",
            "/workspace/crates/running-process/src/async_process.rs"
        ));
    }
}

#[test]
fn ui() {
    dylint_testing::ui_test(env!("CARGO_PKG_NAME"), "ui");
}
