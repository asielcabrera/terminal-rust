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
