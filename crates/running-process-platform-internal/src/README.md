# Source layout

`lib.rs` owns the single `std::cfg_select!` host choice.  `platform.rs` and
`platform/` name neutral capabilities only.  Native code belongs below one of
the private platform roots selected by `lib.rs`.
