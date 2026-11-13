use super::*;

const WINDOW: Rect = Rect::new(0, 0, 801, 601);
const LIMITS: Limits = Limits {
    min_w: 80,
    min_h: 60,
};
const PARAMS: HitParams = HitParams {
    divider_zone: 4.0,
    edge_band: 6.0,
    move_ring: 12.0,
};

fn id(n: u32) -> PaneId {
    PaneId(n)
}

fn rect_of(layout: &Layout, pane: u32) -> Rect {
    layout
        .rects(WINDOW)
        .into_iter()
        .find(|(candidate, _)| *candidate == id(pane))
        .unwrap()
        .1
}

/// 1 | 2 in a row
fn two_columns() -> Layout {
    let mut layout = Layout::new(id(1));
    assert!(layout.split(id(1), Axis::Horizontal, id(2), WINDOW, LIMITS));
    layout
}

/// 2x2 grid: top 1 | 2, bottom 3 | 4
fn grid() -> Layout {
    let mut layout = Layout::new(id(1));
    assert!(layout.split(id(1), Axis::Vertical, id(3), WINDOW, LIMITS));
    assert!(layout.split(id(1), Axis::Horizontal, id(2), WINDOW, LIMITS));
    assert!(layout.split(id(3), Axis::Horizontal, id(4), WINDOW, LIMITS));
    layout
}

#[test]
fn a_single_pane_fills_the_window() {
    let layout = Layout::new(id(1));
    assert_eq!(layout.rects(WINDOW), vec![(id(1), WINDOW)]);
    assert!(layout.dividers(WINDOW).is_empty());
}

#[test]
fn splitting_shares_the_space_exactly_with_odd_sizes() {
    let layout = two_columns();
    let (a, b) = (rect_of(&layout, 1), rect_of(&layout, 2));
    assert_eq!(a.w + b.w, WINDOW.w);
    assert_eq!(b.x, a.right());
    assert_eq!((a.h, b.h), (WINDOW.h, WINDOW.h));
    // At 801 px, the first one gets the rounded-up half
    assert_eq!((a.w, b.w), (401, 400));
}

#[test]
fn nested_splits_cover_the_window_without_gaps_or_overlaps() {
    let layout = grid();
    let mut covered = vec![0u8; (WINDOW.w * WINDOW.h) as usize];
    for (_, rect) in layout.rects(WINDOW) {
        for y in rect.y..rect.bottom() {
            for x in rect.x..rect.right() {
                covered[(y * WINDOW.w + x) as usize] += 1;
            }
        }
    }
    assert!(covered.iter().all(|&count| count == 1));
}

#[test]
fn the_new_pane_gets_the_focus() {
    let layout = two_columns();
    assert_eq!(layout.focus(), id(2));
}

#[test]
fn splitting_is_refused_when_the_halves_would_be_too_small() {
    let mut layout = Layout::new(id(1));
    let tiny = Rect::new(0, 0, 150, 600);
    assert!(!layout.split(id(1), Axis::Horizontal, id(2), tiny, LIMITS));
    assert_eq!(layout.tiled_ids(), vec![id(1)]);
    assert!(layout.split(id(1), Axis::Vertical, id(2), tiny, LIMITS));
    // And a pane that does not exist cannot be split
    assert!(!layout.split(id(9), Axis::Vertical, id(3), tiny, LIMITS));
}

#[test]
fn closing_gives_the_space_to_the_sibling_and_moves_the_focus_back() {
    let mut layout = two_columns();
    assert_eq!(layout.close(id(2)), Closed::Pane);
    assert_eq!(layout.rects(WINDOW), vec![(id(1), WINDOW)]);
    assert_eq!(layout.focus(), id(1));
    assert_eq!(layout.close(id(1)), Closed::LastTiled);
    assert_eq!(layout.close(id(7)), Closed::Unknown);
}

#[test]
fn closing_a_nested_pane_only_grows_its_sibling() {
    let mut layout = grid();
    let before = rect_of(&layout, 3);
    assert_eq!(layout.close(id(4)), Closed::Pane);
    assert_eq!(rect_of(&layout, 3).w, WINDOW.w);
    assert_eq!(rect_of(&layout, 3).y, before.y);
    assert_eq!(rect_of(&layout, 1), rect_of(&grid(), 1));
}

#[test]
fn focus_moves_to_the_neighbour_in_the_direction() {
    let mut layout = grid();
    assert!(layout.set_focus(id(1)));
    assert!(layout.focus_dir(Dir::Right, WINDOW));
    assert_eq!(layout.focus(), id(2));
    assert!(layout.focus_dir(Dir::Down, WINDOW));
    assert_eq!(layout.focus(), id(4));
    assert!(layout.focus_dir(Dir::Left, WINDOW));
    assert_eq!(layout.focus(), id(3));
    assert!(layout.focus_dir(Dir::Up, WINDOW));
    assert_eq!(layout.focus(), id(1));
    // At the edge there is nowhere to go
    assert!(!layout.focus_dir(Dir::Left, WINDOW));
    assert!(!layout.focus_dir(Dir::Up, WINDOW));
    assert_eq!(layout.focus(), id(1));
}

#[test]
fn with_two_neighbours_the_most_recent_one_wins_a_tie() {
    // 1 on the left, 2 and 3 stacked on the right. With an even window height both overlap
    // 1 equally and are equally close: the one that last had focus wins
    let window = Rect::new(0, 0, 800, 600);
    let mut layout = Layout::new(id(1));
    layout.split(id(1), Axis::Horizontal, id(2), window, LIMITS);
    layout.split(id(2), Axis::Vertical, id(3), window, LIMITS);
    layout.set_focus(id(2));
    layout.set_focus(id(1));
    assert!(layout.focus_dir(Dir::Right, window));
    assert_eq!(layout.focus(), id(2));
    layout.set_focus(id(3));
    layout.set_focus(id(1));
    assert!(layout.focus_dir(Dir::Right, window));
    assert_eq!(layout.focus(), id(3));
}

#[test]
fn with_unequal_overlap_the_pane_sharing_more_edge_wins() {
    // At 601 px tall the top pane is 301 and the bottom one 300
    let mut layout = Layout::new(id(1));
    layout.split(id(1), Axis::Horizontal, id(2), WINDOW, LIMITS);
    layout.split(id(2), Axis::Vertical, id(3), WINDOW, LIMITS);
    layout.set_focus(id(3));
    layout.set_focus(id(1));
    assert!(layout.focus_dir(Dir::Right, WINDOW));
    assert_eq!(layout.focus(), id(2));
}

#[test]
fn cycling_visits_every_pane_in_order_and_wraps() {
    let mut layout = grid();
    layout.set_focus(id(1));
    let mut seen = Vec::new();
    for _ in 0..4 {
        layout.focus_cycle(true);
        seen.push(layout.focus());
    }
    assert_eq!(seen, vec![id(2), id(3), id(4), id(1)]);
    assert!(layout.focus_cycle(false));
    assert_eq!(layout.focus(), id(4));
}

#[test]
fn growing_a_pane_moves_its_border_and_respects_the_minimum() {
    let mut layout = two_columns();
    let before = rect_of(&layout, 1).w;
    assert!(layout.resize(id(1), Dir::Right, 50, WINDOW, LIMITS));
    assert_eq!(rect_of(&layout, 1).w, before + 50);
    // A pane flush with the right edge "grows to the right" by moving its left edge
    let width = rect_of(&layout, 2).w;
    assert!(layout.resize(id(2), Dir::Right, 20, WINDOW, LIMITS));
    assert_eq!(
        rect_of(&layout, 2).w,
        width - 20,
        "shrinks: it has no right edge"
    );
    // Toward the opposite side it grows
    assert!(layout.resize(id(2), Dir::Left, 20, WINDOW, LIMITS));
    assert_eq!(rect_of(&layout, 2).w, width);
    // Never below the minimum
    assert!(layout.resize(id(1), Dir::Right, 10_000, WINDOW, LIMITS));
    assert_eq!(rect_of(&layout, 2).w, LIMITS.min_w);
    assert!(!layout.resize(id(1), Dir::Right, 10, WINDOW, LIMITS));
}

#[test]
fn resizing_looks_for_the_nearest_split_along_the_right_axis() {
    // Columns 1 | (2 over 3): resizing 3 vertically moves the 2/3 divider
    let mut layout = Layout::new(id(1));
    layout.split(id(1), Axis::Horizontal, id(2), WINDOW, LIMITS);
    layout.split(id(2), Axis::Vertical, id(3), WINDOW, LIMITS);
    let (top, bottom) = (rect_of(&layout, 2).h, rect_of(&layout, 3).h);
    assert!(layout.resize(id(2), Dir::Down, 30, WINDOW, LIMITS));
    assert_eq!(rect_of(&layout, 2).h, top + 30);
    assert_eq!(rect_of(&layout, 3).h, bottom - 30);
    // And horizontally the only divider is the column one
    let width = rect_of(&layout, 1).w;
    assert!(layout.resize(id(3), Dir::Left, 10, WINDOW, LIMITS));
    assert_eq!(rect_of(&layout, 1).w, width - 10);
    // With a single pane there is nothing to move
    assert!(!Layout::new(id(1)).resize(id(1), Dir::Right, 10, WINDOW, LIMITS));
}

#[test]
fn nested_minimums_are_respected_when_moving_a_divider() {
    // 1 | (2 | 3): the divider of 1 cannot go past leaving minimum room for 2 and 3
    let mut layout = Layout::new(id(1));
    layout.split(id(1), Axis::Horizontal, id(2), WINDOW, LIMITS);
    layout.split(id(2), Axis::Horizontal, id(3), WINDOW, LIMITS);
    layout.resize(id(1), Dir::Right, 10_000, WINDOW, LIMITS);
    assert_eq!(
        rect_of(&layout, 2).w + rect_of(&layout, 3).w,
        2 * LIMITS.min_w
    );
    assert!(rect_of(&layout, 2).w >= LIMITS.min_w && rect_of(&layout, 3).w >= LIMITS.min_w);
}

#[test]
fn dragging_a_divider_follows_the_pointer_and_clamps() {
    let mut layout = two_columns();
    let path = layout.dividers(WINDOW)[0].path.clone();
    assert!(layout.set_divider(&path, 300, WINDOW, LIMITS));
    assert_eq!(rect_of(&layout, 1).w, 300);
    assert!(layout.set_divider(&path, -50, WINDOW, LIMITS));
    assert_eq!(rect_of(&layout, 1).w, LIMITS.min_w);
    assert!(layout.set_divider(&path, 5000, WINDOW, LIMITS));
    assert_eq!(rect_of(&layout, 2).w, LIMITS.min_w);
}

#[test]
fn ratios_survive_a_window_resize() {
    let mut layout = two_columns();
    let big = Rect::new(0, 0, 1602, 1202);
    let a = layout.rects(big)[0].1;
    assert_eq!(a.w, 801);
    layout.resize(id(1), Dir::Right, 0, big, LIMITS);
    assert_eq!(layout.rects(big)[0].1.w, 801);
}

#[test]
fn zoom_shows_only_the_focused_pane_and_is_undone_by_layout_changes() {
    let mut layout = two_columns();
    assert!(layout.toggle_zoom());
    assert_eq!(layout.rects(WINDOW), vec![(id(2), WINDOW)]);
    assert!(layout.dividers(WINDOW).is_empty());
    assert_eq!(layout.visible_count(), 1);
    assert!(layout.toggle_zoom());
    assert_eq!(layout.rects(WINDOW).len(), 2);

    // Moving focus, splitting or closing clears it
    layout.toggle_zoom();
    assert!(layout.focus_dir(Dir::Left, WINDOW));
    assert_eq!(layout.zoomed(), None);
    layout.toggle_zoom();
    assert!(layout.split(id(1), Axis::Vertical, id(3), WINDOW, LIMITS));
    assert_eq!(layout.zoomed(), None);
    assert_eq!(layout.rects(WINDOW).len(), 3);
    layout.toggle_zoom();
    layout.close(id(3));
    assert_eq!(layout.zoomed(), None);

    // With a single pane there is nothing to zoom
    assert!(!Layout::new(id(1)).toggle_zoom());
}

#[test]
fn visible_panes_are_listed_from_the_bottom_up() {
    let mut layout = two_columns();
    layout.add_float(id(3), 0.5, 0.5);
    assert_eq!(layout.visible_ids(), vec![id(1), id(2), id(3)]);
    layout.toggle_zoom();
    // With zoom on, a floating pane is not a tiled pane: it cannot be zoomed
    assert_eq!(layout.zoomed(), None);
    layout.set_focus(id(2));
    layout.toggle_zoom();
    assert_eq!(layout.visible_ids(), vec![id(2), id(3)]);
    layout.toggle_floats();
    assert_eq!(layout.visible_ids(), vec![id(2)]);
}

#[test]
fn a_new_float_is_centered_and_cascades() {
    let mut layout = Layout::new(id(1));
    layout.add_float(id(2), 0.5, 0.5);
    let first = layout.float_rect(id(2), WINDOW).unwrap();
    // Half of 801x601, rounded
    assert_eq!((first.w, first.h), (401, 301));
    assert_eq!((first.x, first.y), (200, 150));
    layout.add_float(id(3), 0.5, 0.5);
    let second = layout.float_rect(id(3), WINDOW).unwrap();
    assert!(second.x > first.x && second.y > first.y);
    assert_eq!(layout.focus(), id(3));
    assert!(layout.is_float(id(3)) && !layout.is_float(id(1)));
    assert_eq!(layout.tiled_ids(), vec![id(1)]);
    assert_eq!(layout.all_ids(), vec![id(1), id(2), id(3)]);
}

#[test]
fn floats_stay_inside_the_window_and_keep_their_proportions_on_resize() {
    let mut layout = Layout::new(id(1));
    layout.add_float(id(2), 0.5, 0.5);
    assert!(layout.move_float(id(2), 10_000, 10_000, WINDOW));
    let rect = layout.float_rect(id(2), WINDOW).unwrap();
    assert_eq!((rect.right(), rect.bottom()), (WINDOW.w, WINDOW.h));
    assert!(layout.move_float(id(2), -10_000, -10_000, WINDOW));
    let rect = layout.float_rect(id(2), WINDOW).unwrap();
    assert_eq!((rect.x, rect.y), (0, 0));
    assert!(
        !layout.move_float(id(2), -5, -5, WINDOW),
        "already in the corner"
    );
    let doubled = Rect::new(0, 0, 1602, 1202);
    let big = layout.float_rect(id(2), doubled).unwrap();
    assert!((i64::from(big.w) - 2 * i64::from(rect.w)).abs() <= 2);
}

#[test]
fn dragging_float_edges_resizes_within_limits() {
    let mut layout = Layout::new(id(1));
    layout.add_float(id(2), 0.5, 0.5);
    let start = layout.float_rect(id(2), WINDOW).unwrap();
    let right = Edge {
        right: true,
        ..Edge::default()
    };
    assert!(layout.resize_float_edge(id(2), right, 40, 0, WINDOW, LIMITS));
    assert_eq!(layout.float_rect(id(2), WINDOW).unwrap().w, start.w + 40);
    let top_left = Edge {
        left: true,
        top: true,
        ..Edge::default()
    };
    assert!(layout.resize_float_edge(id(2), top_left, -20, -10, WINDOW, LIMITS));
    let now = layout.float_rect(id(2), WINDOW).unwrap();
    assert_eq!((now.x, now.y), (start.x - 20, start.y - 10));
    // It does not go below the minimum or leave the window
    layout.resize_float_edge(id(2), right, -10_000, 0, WINDOW, LIMITS);
    assert_eq!(layout.float_rect(id(2), WINDOW).unwrap().w, LIMITS.min_w);
    layout.resize_float_edge(id(2), right, 10_000, 0, WINDOW, LIMITS);
    assert_eq!(layout.float_rect(id(2), WINDOW).unwrap().right(), WINDOW.w);
}

#[test]
fn resizing_a_float_with_keys_grows_right_and_down_and_shrinks_left_and_up() {
    let mut layout = Layout::new(id(1));
    layout.add_float(id(2), 0.5, 0.5);
    let start = layout.float_rect(id(2), WINDOW).unwrap();
    assert!(layout.resize(id(2), Dir::Right, 20, WINDOW, LIMITS));
    assert!(layout.resize(id(2), Dir::Up, 15, WINDOW, LIMITS));
    let now = layout.float_rect(id(2), WINDOW).unwrap();
    assert_eq!(
        (now.w, now.h, now.x, now.y),
        (start.w + 20, start.h - 15, start.x, start.y)
    );
}

#[test]
fn hiding_floats_returns_the_focus_to_the_tiles_and_showing_restores_it() {
    let mut layout = two_columns();
    layout.set_focus(id(1));
    layout.add_float(id(3), 0.5, 0.5);
    assert_eq!(layout.focus(), id(3));
    assert!(layout.toggle_floats());
    assert!(!layout.floats_visible());
    assert_eq!(layout.focus(), id(1));
    assert!(layout.float_rects(WINDOW).is_empty());
    assert_eq!(layout.visible_count(), 2);
    assert!(layout.toggle_floats());
    assert!(layout.floats_visible());
    assert_eq!(layout.focus(), id(3));
    // Without floating panes there is nothing to toggle
    assert!(!two_columns().toggle_floats());
}

#[test]
fn focusing_a_hidden_float_shows_the_floats() {
    let mut layout = Layout::new(id(1));
    layout.add_float(id(2), 0.5, 0.5);
    layout.toggle_floats();
    assert!(layout.set_focus(id(2)));
    assert!(layout.floats_visible());
}

#[test]
fn closing_a_float_gives_the_focus_back() {
    let mut layout = two_columns();
    layout.set_focus(id(1));
    layout.add_float(id(3), 0.5, 0.5);
    assert_eq!(layout.close(id(3)), Closed::Pane);
    assert_eq!(layout.focus(), id(1));
    assert!(!layout.has_floats());
}

#[test]
fn splits_from_a_float_go_to_the_last_tiled_pane() {
    let mut layout = two_columns();
    layout.set_focus(id(1));
    layout.add_float(id(3), 0.5, 0.5);
    assert_eq!(layout.split_target(), id(1));
    assert!(
        !layout.split(id(3), Axis::Horizontal, id(4), WINDOW, LIMITS),
        "a floating pane cannot be split"
    );
}

#[test]
fn raising_a_float_changes_the_stacking_order() {
    let mut layout = Layout::new(id(1));
    layout.add_float(id(2), 0.5, 0.5);
    layout.add_float(id(3), 0.5, 0.5);
    let order = |layout: &Layout| {
        layout
            .float_rects(WINDOW)
            .into_iter()
            .map(|(id, _)| id)
            .collect::<Vec<_>>()
    };
    assert_eq!(order(&layout), vec![id(2), id(3)]);
    layout.set_focus(id(2));
    assert_eq!(order(&layout), vec![id(3), id(2)]);
}

#[test]
fn cycling_includes_visible_floats_after_the_tiles() {
    let mut layout = two_columns();
    layout.add_float(id(3), 0.5, 0.5);
    layout.set_focus(id(2));
    layout.focus_cycle(true);
    assert_eq!(layout.focus(), id(3));
    layout.focus_cycle(true);
    assert_eq!(layout.focus(), id(1));
    layout.toggle_floats();
    layout.set_focus(id(2));
    layout.focus_cycle(true);
    assert_eq!(layout.focus(), id(1));
}

#[test]
fn hit_testing_prefers_floats_then_dividers_then_panes() {
    let mut layout = two_columns();
    let divider_x = f64::from(rect_of(&layout, 1).right());
    assert_eq!(
        layout.hit_test(divider_x + 2.0, 100.0, WINDOW, PARAMS),
        Hit::Divider(vec![], Axis::Horizontal)
    );
    assert_eq!(
        layout.hit_test(divider_x - 100.0, 100.0, WINDOW, PARAMS),
        Hit::Pane(id(1))
    );
    assert_eq!(
        layout.hit_test(divider_x + 100.0, 100.0, WINDOW, PARAMS),
        Hit::Pane(id(2))
    );
    assert_eq!(layout.hit_test(-1.0, 10.0, WINDOW, PARAMS), Hit::None);

    layout.add_float(id(3), 0.5, 0.5);
    let float = layout.float_rect(id(3), WINDOW).unwrap();
    let (cx, cy) = (
        f64::from(float.x + float.w / 2),
        f64::from(float.y + float.h / 2),
    );
    assert_eq!(layout.hit_test(cx, cy, WINDOW, PARAMS), Hit::Pane(id(3)));
    // The ring moves, the edge band resizes
    assert_eq!(
        layout.hit_test(f64::from(float.x) + 8.0, cy, WINDOW, PARAMS),
        Hit::FloatMove(id(3))
    );
    assert_eq!(
        layout.hit_test(f64::from(float.x) + 2.0, cy, WINDOW, PARAMS),
        Hit::FloatResize(
            id(3),
            Edge {
                left: true,
                ..Edge::default()
            }
        )
    );
    assert_eq!(
        layout.hit_test(
            f64::from(float.right()) - 1.0,
            f64::from(float.bottom()) - 1.0,
            WINDOW,
            PARAMS
        ),
        Hit::FloatResize(
            id(3),
            Edge {
                right: true,
                bottom: true,
                ..Edge::default()
            }
        )
    );
    // A divider covered by the floating pane cannot be grabbed through it
    let hidden_divider = layout.hit_test(divider_x, cy, WINDOW, PARAMS);
    assert!(matches!(
        hidden_divider,
        Hit::Pane(_) | Hit::FloatMove(_) | Hit::FloatResize(..)
    ));
    // And with the floating panes hidden, what is underneath shows again
    layout.toggle_floats();
    assert_eq!(
        layout.hit_test(divider_x, cy, WINDOW, PARAMS),
        Hit::Divider(vec![], Axis::Horizontal)
    );
}

#[test]
fn divider_lines_are_centered_on_the_boundary_and_span_the_split() {
    let layout = two_columns();
    let divider = &layout.dividers(WINDOW)[0];
    let line = divider.rect(2);
    assert_eq!(line, Rect::new(divider.pos - 1, 0, 2, WINDOW.h));
    let mut stacked = Layout::new(id(1));
    stacked.split(id(1), Axis::Vertical, id(2), WINDOW, LIMITS);
    let divider = &stacked.dividers(WINDOW)[0];
    assert_eq!(divider.rect(1), Rect::new(0, divider.pos, WINDOW.w, 1));
}

#[test]
fn a_zoomed_pane_has_no_dividers_to_grab() {
    let mut layout = two_columns();
    layout.toggle_zoom();
    let x = f64::from(WINDOW.w / 2);
    assert_eq!(layout.hit_test(x, 100.0, WINDOW, PARAMS), Hit::Pane(id(2)));
}

#[test]
fn split_sizes_never_returns_an_empty_child_when_there_is_room() {
    for total in [2u32, 3, 10, 801] {
        for ratio in [0.0, 0.001, 0.5, 0.999, 1.0] {
            let (a, b) = split_sizes(total, ratio);
            assert_eq!(a + b, total);
            assert!(a >= 1 && b >= 1, "total={total} ratio={ratio} -> {a}+{b}");
        }
    }
}

#[test]
fn split_preview_predicts_exactly_what_the_split_will_do() {
    let mut layout = Layout::new(id(1));
    let preview = layout
        .split_preview(id(1), Axis::Horizontal, WINDOW, LIMITS)
        .expect("the split fits");
    assert!(layout.split(id(1), Axis::Horizontal, id(2), WINDOW, LIMITS));
    assert_eq!(rect_of(&layout, 2), preview);

    // With zoom on, the preview still sees the real rectangles
    layout.toggle_zoom();
    let preview = layout
        .split_preview(id(1), Axis::Vertical, WINDOW, LIMITS)
        .expect("the split fits");
    assert!(layout.split(id(1), Axis::Vertical, id(3), WINDOW, LIMITS));
    assert_eq!(rect_of(&layout, 3), preview);

    // A refused split has no preview, and a pane that does not exist either
    let tiny = Rect::new(0, 0, 150, 600);
    assert!(
        Layout::new(id(1))
            .split_preview(id(1), Axis::Horizontal, tiny, LIMITS)
            .is_none()
    );
    assert!(!Layout::new(id(1)).split(id(1), Axis::Horizontal, id(2), tiny, LIMITS));
    assert!(
        layout
            .split_preview(id(9), Axis::Vertical, WINDOW, LIMITS)
            .is_none()
    );
}

#[test]
fn float_preview_predicts_exactly_what_add_float_will_do() {
    let mut layout = Layout::new(id(1));
    let first = layout.float_preview(0.5, 0.5, WINDOW);
    layout.add_float(id(2), 0.5, 0.5);
    assert_eq!(layout.float_rect(id(2), WINDOW).unwrap(), first);
    // The second one accounts for the cascade offset of the first
    let second = layout.float_preview(0.5, 0.5, WINDOW);
    layout.add_float(id(3), 0.5, 0.5);
    assert_eq!(layout.float_rect(id(3), WINDOW).unwrap(), second);
    assert!(second.x > first.x && second.y > first.y);
}

#[test]
#[should_panic(expected = "projection is read-only")]
fn a_projection_refuses_mutations() {
    let layout = grid();
    let state = layout.state();
    let mut projection = Layout::from_state(state);
    // Mutating a projection would diverge it from the server's authority
    projection.set_focus(id(1));
}

#[test]
fn a_layout_survives_a_state_round_trip() {
    let mut layout = grid();
    layout.add_float(id(5), 0.5, 0.5);
    layout.toggle_floats();
    layout.set_focus(id(1));
    layout.toggle_zoom();

    let state = layout.state();
    let rebuilt = Layout::from_state(state.clone());
    assert_eq!(rebuilt.state(), state);
    // The pure queries of the projection match the original
    assert_eq!(rebuilt.rects(WINDOW), layout.rects(WINDOW));
    assert_eq!(rebuilt.focus(), layout.focus());
    assert_eq!(rebuilt.zoomed(), layout.zoomed());
    assert_eq!(rebuilt.visible_ids(), layout.visible_ids());
    assert_eq!(rebuilt.split_target(), layout.split_target());
}
