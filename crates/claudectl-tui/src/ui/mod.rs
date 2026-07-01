// `brain` (full-screen Brain Review surface) stays in the binary crate as
// `src/brain_screen.rs` — it depends on `brain::metrics` and `brain::risk`
// which are binary-only modules. main.rs calls it directly.
pub mod demo_tour;
pub mod detail;
pub mod help;
#[cfg(feature = "relay")]
pub mod peers;
pub mod skills;
pub mod status_bar;
#[cfg(feature = "coord")]
pub mod supervisor;
pub mod table;
