use super::*;

fn listing(pane: u32, alive: bool) -> wire::AutomationPane {
    wire::AutomationPane {
        pane: PaneId(pane),
        block: BlockId(u64::from(pane)),
        cols: 80,
        rows: 24,
        alive,
    }
}

#[test]
fn the_cli_resolves_a_pane_to_its_block() {
    let panes = vec![listing(0, true), listing(1, false)];
    assert_eq!(
        resolve_pane(&panes, 0).expect("the alive pane resolves"),
        (PaneId(0), BlockId(0))
    );
    let error = resolve_pane(&panes, 1).expect_err("a dead pane is refused");
    assert!(error.contains("exited"), "the error says why: {error}");
    let error = resolve_pane(&panes, 7).expect_err("an unknown pane is refused");
    assert!(error.contains("7"), "the error names the pane: {error}");
}

#[test]
fn the_ls_table_is_sorted_and_reports_state() {
    let panes = vec![listing(3, true), listing(12, false), listing(0, true)];
    let table = ls_table(&panes);
    let lines: Vec<_> = table.lines().collect();
    assert_eq!(lines[0], "pane  block  size  state");
    assert_eq!(lines[1], "0     0      80x24  alive");
    assert_eq!(lines[2], "3     3      80x24  alive");
    assert_eq!(lines[3], "12    12     80x24  dead");
    assert_eq!(lines.len(), 4, "no trailing rows");
}

#[test]
fn terminal_happenings_map_to_well_known_wire_emits() {
    // A bell broadcasts as a well-known event with the pane that rang it
    let op = well_known_emit(PaneId(3), &LuaEvent::Bell).unwrap();
    assert!(matches!(
        op,
        wire::WireOp::Emit { name, payload, depth }
            if name == "bell" && depth == 0 && payload == serde_json::json!({ "pane": 3 })
    ));
    // A title change carries the pane and the title
    let op = well_known_emit(PaneId(1), &LuaEvent::TitleChanged("editor".to_string())).unwrap();
    assert!(matches!(
        op,
        wire::WireOp::Emit { name, payload, depth }
            if name == "title-changed"
                && depth == 0
                && payload == serde_json::json!({ "pane": 1, "title": "editor" })
    ));
}

// T12: which whole lines a Delta scroll actually moves, and which pre-scroll viewport row
// slides out during the animated slide (the edge the content leaves through)
#[test]
fn a_delta_scroll_moves_what_the_scrollback_allows_and_slides_out_one_edge_row() {
    // (total, offset, len) of the scrollable area: 100 history rows above a 24-row viewport
    let bar = (124, 100, 24);
    // Scrolling up 3 lines moves 3 rows; the slide starts with the pre-scroll bottom rows
    // still visible, so the row that slides out is row 21 (rows 22 and 23 left with the
    // instant part of the jump)
    assert_eq!(smooth_slide(-3, bar, 24), Some((true, Some(21))));
    // Scrolling down 1 line slides out the viewport's top row
    assert_eq!(smooth_slide(1, (124, 0, 24), 24), Some((false, Some(0))));
}

#[test]
fn a_scroll_clamped_at_a_scrollback_edge_does_not_animate() {
    // At the top of the history there is nothing above to scroll into
    assert_eq!(smooth_slide(-3, (124, 0, 24), 24), None);
    // At the bottom (the resting viewport) there is nothing below
    assert_eq!(smooth_slide(2, (124, 100, 24), 24), None);
}

#[test]
fn a_jump_bigger_than_the_viewport_still_animates_but_has_no_ghost_row() {
    // 30 lines up from 100 rows of room: the slide-out row would be row -6, outside the
    // pre-scroll viewport, so no ghost is captured (the animation still runs)
    assert_eq!(smooth_slide(-30, (124, 100, 24), 24), Some((true, None)));
    assert_eq!(smooth_slide(30, (124, 0, 24), 24), Some((false, None)));
}
