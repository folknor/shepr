use shepr_protocol::PaneSurfaceFrame;

struct Fnv64(u64);

impl Fnv64 {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn write_byte(&mut self, byte: u8) {
        self.0 ^= u64::from(byte);
        self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.write_byte(*byte);
        }
    }

    fn write_pane_id(&mut self, id: shepr_protocol::PublicPaneId) {
        self.write(&id.workspace_id().number().to_le_bytes());
        self.write(&id.number().get().to_le_bytes());
    }

    fn finish(self) -> u64 {
        self.0
    }
}

pub(in crate::shell) fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = Fnv64::new();
    hash.write(bytes);
    hash.finish()
}

/// Classify a pane frame against a split in server surface coordinates.
pub(in crate::shell) fn pane_split_side(
    rect: shepr_protocol::SurfaceRect,
    split: &shepr_protocol::PaneSurfaceSplit,
) -> Option<shepr_core::geometry::SplitBranch> {
    let area = split.area;
    if rect.x < area.x
        || rect.y < area.y
        || rect.x.saturating_add(rect.width) > area.x.saturating_add(area.width)
        || rect.y.saturating_add(rect.height) > area.y.saturating_add(area.height)
    {
        return None;
    }
    let first = match split.direction {
        shepr_protocol::PaneSurfaceSplitDirection::Horizontal => rect.x < split.pos,
        shepr_protocol::PaneSurfaceSplitDirection::Vertical => rect.y < split.pos,
    };
    Some(if first {
        shepr_core::geometry::SplitBranch::First
    } else {
        shepr_core::geometry::SplitBranch::Second
    })
}

pub(in crate::shell) fn pane_surface_topology_signature(surface: &PaneSurfaceFrame) -> u64 {
    fn write_delimited(hash: &mut Fnv64, bytes: &[u8]) {
        hash.write(bytes);
        hash.write_byte(0xff);
    }

    let mut panes = surface.panes.iter().collect::<Vec<_>>();
    panes.sort_by_key(|pane| pane.pane_id);
    let mut hash = Fnv64::new();
    for pane in &panes {
        hash.write_pane_id(pane.pane_id);
    }
    let mut splits = surface.splits.iter().collect::<Vec<_>>();
    splits.sort_by(|left, right| left.path.cmp(&right.path));
    for split in splits {
        write_delimited(
            &mut hash,
            &[match split.direction {
                shepr_protocol::PaneSurfaceSplitDirection::Horizontal => 0,
                shepr_protocol::PaneSurfaceSplitDirection::Vertical => 1,
            }],
        );
        for branch in &split.path {
            hash.write_byte(u8::from(
                *branch == shepr_core::geometry::SplitBranch::Second,
            ));
        }
        hash.write_byte(0xff);
        // Ratio movement changes pane rectangles but should preserve child membership.
        for pane in &panes {
            hash.write_pane_id(pane.pane_id);
            let child = match pane_split_side(pane.rect, split) {
                Some(shepr_core::geometry::SplitBranch::First) => 0,
                Some(shepr_core::geometry::SplitBranch::Second) => 1,
                None => 2,
            };
            write_delimited(&mut hash, &[child]);
        }
    }
    hash.finish()
}

#[cfg(test)]
mod tests {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    use super::pane_surface_topology_signature;

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
