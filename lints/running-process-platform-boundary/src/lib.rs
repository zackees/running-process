#![feature(rustc_private)]

extern crate rustc_hir;

use dylint_linting::declare_late_lint;
use rustc_hir::{def::Res, Expr, ExprKind};
use rustc_lint::{LateContext, LateLintPass};

const RAW_APIS: [&str; 3] = [
    "tokio::process::Command",
    "tokio::process::Child",
    "std::process::Stdio",
];

declare_late_lint! {
    /// ### What it does
    ///
    /// Rejects a raw process API recorded in `raw_platform_apis.toml` outside
    /// `running-process-platform-internal`.
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
        let Res::Def(_, def_id) = cx.qpath_res(qpath, expr.hir_id) else {
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

        cx.tcx.dcx().span_err(
            expr.span,
            format!(
                "forbidden raw platform API `{raw_api}`; use a blessed running_process_platform_internal capability listed in blessed_api.toml"
            ),
        );
    }
}

#[test]
fn ui() {
    dylint_testing::ui_test(env!("CARGO_PKG_NAME"), "ui");
}
