# Mockup ("Planor scroll states") Core Test Matrix & Open Decisions (3.3 / 9.8)

Because the Chrome mockup canvas (`https://claude.ai/artifact/XtUGKiabww34dA5VPfHrPj#page-dcb81aee8d83`) requires Claude authentication not available inside automated browser checks, this matrix encodes every state specified in `docs/TOOLING_PLAN.md` (section 3.3 & 9.8) and `docs/TEST_PLAN.md`, stored in percent-based form in `docs/scroll_states_spec.json` and executed by `tests/core.rs`.

## Active Passing States (`cargo test --test core`)

| Spec / Mockup State | Test Function (`tests/core.rs`) | Verdict |
|---|---|---|
| `landscape_0_windows` .. `landscape_6_windows` | `mockup_landscape_0_to_6_windows` | PASS |
| `portrait_0_windows` .. `portrait_6_windows` | `mockup_portrait_0_to_6_windows` | PASS |
| `landscape_group_2`, `landscape_group_3`, `landscape_group_4` | `mockup_groups_2_to_4_landscape_and_portrait` | PASS |
| `portrait_group_2`, `portrait_group_3`, `portrait_group_4` | `mockup_groups_2_to_4_landscape_and_portrait` | PASS |
| `group_roll_fraction_positive/negative/wrap_settle` | `mockup_group_roll_fractions_and_wrap`, `rolling_a_group_is_continuous_and_settles_on_the_nearest_member` | PASS |
| `insert_after_focused_window` | `mockup_insert_after_focused_and_reorder_after_dropped_on` | PASS |
| `pull_out_from_group` | `mockup_pull_out_close_and_fill_gaps` | PASS |
| `close_window_refill` | `mockup_pull_out_close_and_fill_gaps` | PASS |
| `fill_gaps_short_strip` | `mockup_pull_out_close_and_fill_gaps`, `no_empty_space_when_the_columns_do_not_reach_the_end` | PASS |
| `cross_monitor_insert_rule` (strip transfer / before-after) | `mockup_cross_monitor_insert_rule` | PASS |
| Percent-based atlas runner (`docs/scroll_states_spec.json`) | `mockup_spec_states_match_layout` | PASS |

## Code vs Mockup Disagreements (Owner Decision Pending)

Run failing discrepancy tests explicitly with `cargo test --test core -- --ignored`:

| Mockup State | Test Function (`tests/core.rs`) | Discrepancy | Decision Needed |
|---|---|---|---|
| `drop_zone_outer_20_before` / `middle_60_join` / `outer_20_after` | `mockup_drop_zone_outer_20_middle_60_join` (`#[ignore]`) | `TOOLING_PLAN.md` 3.3 specifies outer **20%** (`< 0.20` / `> 0.80`) beside and middle **60%** (`0.20..=0.80`) join. `src/core.rs::drag()` currently uses **25%** (`< 0.25` / `> 0.75`) and **50%** (`0.25..=0.75`), while `src/daemon.rs::drag_update()` further narrows stacking to `+-stack_center_px` (`20 px`) from center (and `tests/scn_drag.rs` drops at `0.10` / `centre`). | Keep `0.25`/`0.50` + `stack_center_px`, or adopt `0.20`/`0.60`/`0.20`? |
| `cross_monitor_center_stack` | `mockup_cross_monitor_center_stack_rejected` (`#[ignore]`) | `src/daemon.rs::drag_update()` sets `may_stack = !crossing`, preventing stacking when dragging from monitor A onto the center of a window on monitor B (`planor.example.toml` comment confirms intentional rule). | Keep `!crossing` restriction, or allow stacking on cross-monitor center drop? |

## Updating From the Chrome Canvas

To import exact coordinates directly from the Claude artifact canvas when authenticated, export each frame's `workArea`, `tiles[]` (`x_pct`, `y_pct`, `w_pct`, `h_pct`, `on_screen`), and `focus` into `docs/scroll_states_spec.json` (`schema_version = 1`). `mockup_spec_states_match_layout` checks every `"status": "active"` entry across 1920x1152, 2560x1392, and 1440x2512 geometries.
