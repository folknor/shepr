use super::*;

pub(super) fn pane_surface_topology_signature(surface: &PaneSurfaceFrame) -> u64 {
    // limits-exempt: fixed FNV-1a parameters stay beside the topology hash they define.
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    // limits-exempt: fixed FNV-1a parameters stay beside the topology hash they define.
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    fn write(hash: &mut u64, bytes: &[u8]) {
        for byte in bytes {
            *hash ^= u64::from(*byte);
            *hash = hash.wrapping_mul(PRIME);
        }
        *hash ^= 0xff;
        *hash = hash.wrapping_mul(PRIME);
    }

    let mut panes = surface.panes.iter().collect::<Vec<_>>();
    panes.sort_by(|left, right| left.pane_id.as_bytes().cmp(right.pane_id.as_bytes()));
    let mut hash = OFFSET;
    for pane in &panes {
        write(&mut hash, pane.pane_id.as_bytes());
    }
    let mut splits = surface.splits.iter().collect::<Vec<_>>();
    splits.sort_by(|left, right| left.path.cmp(&right.path));
    for split in splits {
        write(
            &mut hash,
            &[match split.direction {
                shepr_protocol::PaneSurfaceSplitDirection::Horizontal => 0,
                shepr_protocol::PaneSurfaceSplitDirection::Vertical => 1,
            }],
        );
        for branch in &split.path {
            hash ^= u64::from(*branch == shepr_core::geometry::SplitBranch::Second);
            hash = hash.wrapping_mul(PRIME);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(PRIME);
        // Ratio movement changes pane rectangles but should preserve child membership.
        for pane in &panes {
            write(&mut hash, pane.pane_id.as_bytes());
            let rect = pane.rect;
            let area = split.area;
            let inside = rect.x >= area.x
                && rect.y >= area.y
                && rect.x.saturating_add(rect.width) <= area.x.saturating_add(area.width)
                && rect.y.saturating_add(rect.height) <= area.y.saturating_add(area.height);
            let child = if !inside {
                2
            } else {
                match split.direction {
                    shepr_protocol::PaneSurfaceSplitDirection::Horizontal => {
                        u8::from(rect.x >= split.pos)
                    }
                    shepr_protocol::PaneSurfaceSplitDirection::Vertical => {
                        u8::from(rect.y >= split.pos)
                    }
                }
            };
            write(&mut hash, &[child]);
        }
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::pane_surface_topology_signature;
    use ratatui::{buffer::Buffer, layout::Rect};

    fn split_surface() -> shepr_protocol::PaneSurfaceFrame {
        let left = shepr_protocol::SurfaceRect {
            x: 0,
            y: 0,
            width: 40,
            height: 24,
        };
        let right = shepr_protocol::SurfaceRect {
            x: 41,
            y: 0,
            width: 39,
            height: 24,
        };
        shepr_protocol::PaneSurfaceFrame {
            boot_id: crate::tests::test_boot_id("boot"),
            projection_revision: shepr_protocol::ProjectionRevision::new(1),
            surface_revision: shepr_protocol::SurfaceRevision::new(1),
            frame: shepr_protocol::FrameData::from_ratatui_buffer_with_hyperlinks(
                &Buffer::empty(Rect::new(0, 0, 80, 24)),
                None,
                &[],
            ),
            panes: vec![pane("w1:p1", left), pane("w1:p2", right)],
            splits: vec![shepr_protocol::PaneSurfaceSplit {
                direction: shepr_protocol::PaneSurfaceSplitDirection::Horizontal,
                pos: 40,
                area: shepr_protocol::SurfaceRect {
                    x: 0,
                    y: 0,
                    width: 80,
                    height: 24,
                },
                hit_rect: shepr_protocol::SurfaceRect {
                    x: 40,
                    y: 0,
                    width: 1,
                    height: 24,
                },
                path: Vec::new(),
            }],
        }
    }

    fn pane(id: &str, rect: shepr_protocol::SurfaceRect) -> shepr_protocol::PaneSurfacePane {
        shepr_protocol::PaneSurfacePane {
            pane_id: id.parse().expect("pane id"),
            content_revision: 0,
            rect,
            inner_rect: rect,
            scrollbar_rect: None,
            scroll: None,
            focused: false,
            mouse_reporting: false,
            sgr_pixel_mouse: false,
            alternate_screen_active: false,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    #[test]
    fn pane_child_changes_change_the_topology_signature() {
        let original = split_surface();
        let mut swapped = original.clone();
        let left = swapped.panes[0].rect;
        swapped.panes[0].rect = swapped.panes[1].rect;
        swapped.panes[1].rect = left;
        let left_inner = swapped.panes[0].inner_rect;
        swapped.panes[0].inner_rect = swapped.panes[1].inner_rect;
        swapped.panes[1].inner_rect = left_inner;

        assert_ne!(
            pane_surface_topology_signature(&original),
            pane_surface_topology_signature(&swapped)
        );
    }

    #[test]
    fn split_ratio_changes_keep_the_topology_signature() {
        let original = split_surface();
        let mut resized = original.clone();
        resized.splits[0].pos = 42;
        resized.panes[0].rect.width = 42;
        resized.panes[0].inner_rect.width = 40;
        resized.panes[1].rect.x = 43;
        resized.panes[1].rect.width = 37;
        resized.panes[1].inner_rect.x = 44;
        resized.panes[1].inner_rect.width = 35;

        assert_eq!(
            pane_surface_topology_signature(&original),
            pane_surface_topology_signature(&resized)
        );
    }
}
