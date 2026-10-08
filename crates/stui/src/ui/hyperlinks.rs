//! Terminals that draw images also make an address clickable when it is wrapped in an OSC 8
//! hyperlink, and they open it on the person's own machine even when stui runs over SSH.
//! Ratatui counts the width of a cell's text, so the escape cannot ride inside a cell: after a
//! frame is drawn the few link cells are written again, between the open and close sequences.

use ratatui::{backend::Backend, backend::CrosstermBackend, buffer::Cell};
use std::io::{self, Write};

use super::*;

/// One link's cells as drawn this frame.
pub(super) struct LinkCells {
    url: String,
    cells: Vec<(u16, u16, Cell)>,
}

impl Ui {
    /// The web addresses showing on screen, with the cells that draw them, read from the finished
    /// frame. An address hidden behind a later layer is left out.
    pub(super) fn link_cells(&self, buf: &Buffer) -> Vec<LinkCells> {
        let info = self.frame.borrow();
        info.hits
            .iter()
            .enumerate()
            .filter_map(|(index, (rect, hit))| {
                let Hit::Link(url) = hit else { return None };
                if !(url.starts_with("https://") || url.starts_with("http://"))
                    || url.chars().any(char::is_control)
                    || info
                        .covers
                        .iter()
                        .any(|(cover, first)| *first > index && cover.intersects(*rect))
                {
                    return None;
                }
                let rect = rect.intersection(buf.area);
                let cells = (rect.y..rect.bottom())
                    .flat_map(|y| (rect.x..rect.right()).map(move |x| (x, y)))
                    .map(|(x, y)| (x, y, buf[(x, y)].clone()))
                    .collect::<Vec<_>>();
                (!cells.is_empty()).then(|| LinkCells {
                    url: url.clone(),
                    cells,
                })
            })
            .collect()
    }
}

/// Write each link's cells again inside an OSC 8 hyperlink.
pub(super) fn write_links<W: Write>(
    backend: &mut CrosstermBackend<W>,
    links: &[LinkCells],
) -> io::Result<()> {
    for link in links {
        write!(backend, "\x1b]8;;{}\x1b\\", link.url)?;
        backend.draw(link.cells.iter().map(|(x, y, cell)| (*x, *y, cell)))?;
        write!(backend, "\x1b]8;;\x1b\\")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, rc::Rc};

    /// A writer the test can read back after the backend takes it.
    #[derive(Clone, Default)]
    struct Sink(Rc<RefCell<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn links(urls: &[&str]) -> Vec<LinkCells> {
        urls.iter()
            .enumerate()
            .map(|(row, url)| LinkCells {
                url: (*url).into(),
                cells: "abc"
                    .chars()
                    .enumerate()
                    .map(|(x, c)| {
                        let mut cell = Cell::default();
                        cell.set_symbol(&c.to_string());
                        (x as u16 + 2, row as u16, cell)
                    })
                    .collect(),
            })
            .collect()
    }

    #[test]
    fn each_link_is_rewritten_between_an_open_and_a_close() {
        let sink = Sink::default();
        let mut backend = CrosstermBackend::new(sink.clone());
        write_links(&mut backend, &links(&["https://example.com/a", "https://example.com/b"]))
            .unwrap();
        let out = String::from_utf8_lossy(&sink.0.borrow()).into_owned();
        let open_a = out.find("\x1b]8;;https://example.com/a\x1b\\").unwrap();
        let open_b = out.find("\x1b]8;;https://example.com/b\x1b\\").unwrap();
        let closes: Vec<_> = out.match_indices("\x1b]8;;\x1b\\").map(|(at, _)| at).collect();
        assert_eq!(closes.len(), 2, "{out:?}");
        // The text sits between its own open and close, and the second link starts after the
        // first has closed.
        assert!(open_a < closes[0] && closes[0] < open_b && open_b < closes[1]);
        assert!(out[open_a..closes[0]].contains('a') && out[open_a..closes[0]].contains('c'));
    }

    #[test]
    fn an_address_in_a_message_is_marked_with_the_cells_that_draw_it() {
        let mut ui = Ui::new(demo::world());
        let agent = ui.world.agents.items()[0].id.clone();
        ui.world.conversations.insert(
            agent.clone(),
            Load::Ready(vec![Entry {
                id: "message/copper".into(),
                at: "12:00".into(),
                body: Body::Assistant("Copper text https://example.com/notes".into()),
            }]),
        );
        ui.open(&agent);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 48)).unwrap();
        let mut found = Vec::new();
        terminal
            .draw(|frame| {
                ui.render(frame);
                found = ui.link_cells(frame.buffer_mut());
            })
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].url, "https://example.com/notes");
        let drawn: String = found[0].cells.iter().map(|(_, _, cell)| cell.symbol()).collect();
        assert_eq!(drawn, "https://example.com/notes");
    }

    #[test]
    fn only_visible_web_addresses_are_marked() {
        let mut ui = Ui::new(demo::world());
        ui.live = true;
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| ui.render(frame)).unwrap();
        {
            let mut info = ui.frame.borrow_mut();
            let area = Rect::new(0, 1, 20, 1);
            for url in [
                "https://example.com/ok",
                "/srv/not-a-web-address",
                "javascript:alert(1)",
                "https://example.com/\u{1b}bad",
            ] {
                info.hits.push((area, Hit::Link(url.into())));
            }
        }
        let marked = ui.link_cells(terminal.current_buffer_mut());
        let urls: Vec<_> = marked.iter().map(|link| link.url.as_str()).collect();
        assert_eq!(urls, ["https://example.com/ok"]);
    }
}
