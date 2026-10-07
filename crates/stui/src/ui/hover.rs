//! Motion only resolves the last frame's hit map. Paint once, over the finished frame.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Target {
    index: usize,
    rect: Rect,
}

#[derive(Default)]
pub(super) struct Hover {
    pointer: Cell<Option<(u16, u16)>>,
    target: Cell<Option<Target>>,
    pub(super) pressed: Cell<bool>,
}

impl Hover {
    pub(super) fn clear(&self) {
        self.pointer.set(None);
        self.target.set(None);
    }
}

impl Ui {
    /// Begin an opaque layer. Its blank cells also hide hits from earlier layers.
    pub(super) fn cover(&self, rect: Rect) {
        let mut info = self.frame.borrow_mut();
        let first = info.hits.len();
        info.covers.push((rect, first));
    }

    fn hover_target(&self, point: (u16, u16)) -> Option<Target> {
        if self.help || self.hover.pressed.get() {
            return None;
        }
        let info = self.frame.borrow();
        if !contains(info.area, point.0, point.1) {
            return None;
        }
        let first = info
            .covers
            .iter()
            .rev()
            .find(|(rect, _)| contains(*rect, point.0, point.1))
            .map_or(0, |(_, first)| *first);
        info.hits
            .iter()
            .enumerate()
            .skip(first)
            .rev()
            .find(|(_, (rect, _))| contains(*rect, point.0, point.1))
            .map(|(index, (rect, _))| Target { index, rect: *rect })
    }

    /// No cloned Hit, pane, subject, or world lookup on the motion path.
    pub(super) fn mouse_moved(&self, mouse: MouseEvent) -> bool {
        let point = (mouse.column, mouse.row);
        if !self.hover.pressed.get() {
            self.hover.pointer.set(Some(point));
        }
        let next = self.hover_target(point);
        let changed = next != self.hover.target.get();
        if changed {
            self.hover.target.set(next);
        }
        // Terminal-body motion stays with its program. The rendered frame caches its body,
        // avoiding focus/pane allocations and preventing motion through a floating overlay.
        if let Some(body) = self.frame.borrow().terminal_motion
            && contains(body, point.0, point.1)
            && !self.terminal_selecting
            && !self.terminal_selection_mode
            && !mouse
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
            && let Some(native) = self.native_terminal()
            && native
                .mode()
                .intersects(alacritty_terminal::term::TermMode::MOUSE_MODE)
        {
            native.mouse(mouse, point.0 - body.x, point.1 - body.y);
        }
        changed
    }

    pub(super) fn draw_hover(&self, buf: &mut Buffer) {
        // Re-resolve after every draw: scrolling, resizing, or an overlay can replace hits
        // without another motion event. Never reuse an index from an earlier frame.
        let target = self
            .hover
            .pointer
            .get()
            .and_then(|point| self.hover_target(point));
        self.hover.target.set(target);
        let terminal_motion = if self.terminal_focused()
            && !self.help
            && self.popover.is_none()
            && !self.palette_open()
            && !self.home_open()
            && self.context.is_none()
        {
            self.terminal_body.get()
        } else {
            None
        };
        self.frame.borrow_mut().terminal_motion = terminal_motion;
        let Some(target) = target else { return };
        let info = self.frame.borrow();
        let link = matches!(
            info.hits[target.index].1,
            Hit::Link(_) | Hit::Pane(PaneIntent::Open(_))
        );
        // Text to read and select is not a control: it is described in the footer, not lit.
        let plain = matches!(info.hits[target.index].1, Hit::Message | Hit::Subject);
        let rect = if plain {
            Rect::default()
        } else {
            target.rect.intersection(buf.area)
        };
        for y in rect.y..rect.bottom() {
            for x in rect.x..rect.right() {
                if info
                    .covers
                    .iter()
                    .any(|(cover, first)| *first > target.index && contains(*cover, x, y))
                {
                    continue;
                }
                let cell = &mut buf[(x, y)];
                if link {
                    cell.modifier.insert(Modifier::UNDERLINED);
                } else {
                    cell.set_bg(theme::hover_background(cell.bg));
                }
            }
        }
        if let Some(point) = self.hover.pointer.get() {
            self.draw_click_hint(buf, target.rect, &info.hits[target.index].1, point);
        }
    }
}

/// Stable motion must not wake the frame loop early. Other input or a target transition
/// wakes it; the existing timeout still services animation, data, voice and terminal output.
pub(super) fn poll_input(
    timeout: Duration,
    stopping: &std::sync::atomic::AtomicBool,
    mut handle: impl FnMut(Event) -> bool,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if !event::poll(deadline.saturating_duration_since(Instant::now()))? {
            return Ok(());
        }
        let mut changed = false;
        while !stopping.load(std::sync::atomic::Ordering::Relaxed) && !crate::stdin_hung_up() {
            let input = event::read()?;
            let motion =
                matches!(&input, Event::Mouse(mouse) if mouse.kind == MouseEventKind::Moved);
            // Keys (including menu shortcuts and scrolling) and presses paint their new
            // hit map before another click can use it.
            let press = matches!(&input, Event::Mouse(mouse) if matches!(mouse.kind, MouseEventKind::Down(_)))
                || matches!(&input, Event::Key(_));
            let redraw = handle(input);
            if (motion || press) && redraw {
                return Ok(());
            }
            changed |= redraw;
            if Instant::now() >= deadline || !event::poll(Duration::ZERO)? {
                break;
            }
        }
        if changed
            || Instant::now() >= deadline
            || stopping.load(std::sync::atomic::Ordering::Relaxed)
            || crate::stdin_hung_up()
        {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::alloc::{GlobalAlloc, Layout, System};

    thread_local! {
        static ALLOCATIONS: Cell<Option<usize>> = const { Cell::new(None) };
    }

    struct CountedAllocator;

    fn count_allocation() {
        let _ = ALLOCATIONS.try_with(|count| {
            if let Some(n) = count.get() {
                count.set(Some(n + 1));
            }
        });
    }

    // Count only the measured thread, leaving parallel tests and frame construction alone.
    unsafe impl GlobalAlloc for CountedAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            count_allocation();
            unsafe { System.alloc(layout) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            count_allocation();
            unsafe { System.realloc(ptr, layout, size) }
        }
    }

    #[global_allocator]
    static ALLOCATOR: CountedAllocator = CountedAllocator;

    fn motion(x: u16, y: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        })
    }

    #[test]
    fn hover_paints_the_topmost_target_and_links_without_acting() {
        let mut ui = Ui::new(demo::world());
        ui.frame.borrow_mut().area = Rect::new(0, 0, 20, 6);
        ui.hit(Rect::new(0, 0, 10, 1), Hit::Key('s'));
        ui.hit(
            Rect::new(2, 0, 4, 1),
            Hit::Link("https://example.com".into()),
        );
        let focus = ui.focus();
        assert!(ui.input_event(motion(3, 0)));
        assert!(!ui.input_event(motion(4, 0)));
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 6));
        buf.set_style(buf.area, theme::text().bg(theme::BASE));
        ui.draw_hover(&mut buf);
        assert!(buf[(3, 0)].modifier.contains(Modifier::UNDERLINED));
        assert!(!buf[(1, 0)].modifier.contains(Modifier::UNDERLINED));
        assert_eq!(buf[(3, 0)].bg, theme::BASE);
        assert!(ui.input_event(motion(1, 0)));
        ui.draw_hover(&mut buf);
        assert_eq!(buf[(1, 0)].bg, theme::hover_background(theme::BASE));
        assert_eq!(ui.focus(), focus);
        assert!(ui.effects.is_empty() && ui.conversation_state.selection.is_none());
        assert!(
            ui.input_event(motion(20, 0)),
            "outside the window clears hover"
        );
        assert!(ui.hover.target.get().is_none());
    }

    #[test]
    fn opaque_layers_hide_hits_and_clip_a_partly_covered_highlight() {
        let mut ui = Ui::new(demo::world());
        ui.frame.borrow_mut().area = Rect::new(0, 0, 20, 6);
        ui.hit(Rect::new(0, 0, 10, 1), Hit::Tab(1));
        ui.cover(Rect::new(5, 0, 5, 2));
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 6));
        buf.set_style(buf.area, theme::text().bg(theme::BASE));
        ui.input_event(motion(1, 0));
        ui.draw_hover(&mut buf);
        assert_eq!(buf[(1, 0)].bg, theme::hover_background(theme::BASE));
        assert_eq!(buf[(6, 0)].bg, theme::BASE, "the overlay stays untouched");
        assert!(ui.input_event(motion(6, 0)));
        assert!(
            ui.hover.target.get().is_none(),
            "blank overlay cells hide the tab"
        );
        ui.hit(Rect::new(5, 0, 3, 1), Hit::Key('x'));
        assert!(ui.input_event(motion(6, 0)));
        assert_eq!(ui.hover.target.get().unwrap().index, 1);
    }

    #[test]
    fn overlays_drags_focus_loss_and_frame_changes_cannot_keep_old_hover() {
        let mut ui = Ui::new(demo::world());
        ui.glasses = Some(glass::Glasses::open(None, None));
        let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
        terminal.draw(|frame| ui.render(frame)).unwrap();
        assert!(ui.input_event(motion(1, 0)));
        ui.key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
        terminal.draw(|frame| ui.render(frame)).unwrap();
        assert!(
            ui.hover.target.get().is_none(),
            "palette hides underlying bar hits"
        );
        ui.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        terminal.draw(|frame| ui.render(frame)).unwrap();
        assert!(ui.hover.target.get().is_some());
        let body = ui.frame.borrow().glass_leaves[0];
        ui.open_context_menu(body.x, body.y);
        assert!(ui.context.is_some());
        terminal.draw(|frame| ui.render(frame)).unwrap();
        assert!(ui.hover.target.get().is_none(), "menu hides bar hits");
        let menu = ui.frame.borrow().menu.unwrap();
        assert!(ui.input_event(motion(menu.x, menu.y + 1)));
        terminal.draw(|frame| ui.render(frame)).unwrap();
        let target = ui.hover.target.get().unwrap();
        assert!(matches!(
            ui.frame.borrow().hits[target.index].1,
            Hit::Menu(_)
        ));
        assert_eq!(
            terminal.backend().buffer()[(menu.x, menu.y + 1)].bg,
            theme::hover_background(theme::ROW_SELECTED)
        );
        ui.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        terminal.draw(|frame| ui.render(frame)).unwrap();
        ui.input_event(motion(1, 0));
        ui.popover = Some("agent/example".into());
        terminal.draw(|frame| ui.render(frame)).unwrap();
        assert!(ui.hover.target.get().is_none(), "popover hides bar hits");
        ui.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        terminal.draw(|frame| ui.render(frame)).unwrap();
        assert!(ui.hover.target.get().is_some());
        ui.input_event(Event::FocusLost);
        terminal.draw(|frame| ui.render(frame)).unwrap();
        assert!(ui.hover.target.get().is_none());
        ui.input_event(motion(1, 0));
        ui.mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 1,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        terminal.draw(|frame| ui.render(frame)).unwrap();
        assert!(ui.hover.target.get().is_none());
        assert!(!ui.input_event(motion(2, 0)));
        ui.mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 1,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        terminal.draw(|frame| ui.render(frame)).unwrap();
        assert!(
            ui.hover.target.get().is_none(),
            "a drag waits for fresh motion"
        );
        ui.input_event(motion(1, 0));
        ui.frame.borrow_mut().hits.clear();
        let mut buf = Buffer::empty(Rect::new(0, 0, 120, 32));
        ui.draw_hover(&mut buf);
        assert!(
            ui.hover.target.get().is_none(),
            "replaced frame has no old index"
        );
    }

    #[test]
    fn a_full_frame_motion_script_allocates_nothing_and_redraws_per_target_change() {
        let mut ui = Ui::new(demo::world());
        let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
        for _ in 0..2 {
            terminal.draw(|frame| ui.render(frame)).unwrap();
        }
        let expected = {
            let frame = ui.frame.borrow();
            (0..32)
                .flat_map(|y| (0..120).map(move |x| (x, y)))
                .map(|(x, y)| {
                    frame
                        .hits
                        .iter()
                        .enumerate()
                        .rev()
                        .find(|(_, (rect, _))| contains(*rect, x, y))
                        .map(|(index, (rect, _))| (index, *rect))
                })
                .collect::<Vec<_>>()
        };
        let focus = ui.focus();
        let mut last = None;
        let mut changes = 0;
        let mut redraws = 0;
        let mut nanos = 0;
        let mut allocations = 0;
        for _ in 0..10 {
            for (offset, next) in expected.iter().enumerate() {
                changes += usize::from(*next != last);
                last = *next;
                let input = motion((offset % 120) as u16, (offset / 120) as u16);
                ALLOCATIONS.set(Some(0));
                let start = Instant::now();
                let redraw = ui.input_event(input);
                nanos += start.elapsed().as_nanos();
                allocations += ALLOCATIONS.replace(None).unwrap();
                if redraw {
                    redraws += 1;
                    terminal.draw(|frame| ui.render(frame)).unwrap();
                }
            }
        }
        assert_eq!(allocations, 0);
        assert_eq!(redraws, changes);
        assert_eq!(ui.focus(), focus);
        assert!(ui.effects.is_empty() && ui.conversation_state.selection.is_none());
        println!(
            "hover: 38400 events, {changes} target changes, {redraws} redraws, \
                  {allocations} allocations; {:.1} ns/event (timed dispatch only)",
            nanos as f64 / 38400.0
        );
    }
}
