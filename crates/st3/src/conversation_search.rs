//! Embeddable text index over normalized, already-authorized conversations.
//!
//! The caller owns discovery and access control. This disposable SQLite FTS5 index owns
//! no graph facts and never replicates. Replace a source when its normalized entries change.
use anyhow::{Result, ensure};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SearchEntry {
    pub conversation_id: String,
    pub entry_id: String,
    pub agent_id: Option<String>,
    pub timestamp: String,
    pub entry_type: String,
    pub text: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SearchHit {
    pub conversation_id: String,
    pub entry_id: String,
    pub agent_id: Option<String>,
    pub timestamp: String,
    pub entry_type: String,
    pub excerpt: String,
}

/// A single owner's disposable index. Keep it behind a mutex when sharing it.
pub struct SearchIndex {
    connection: Connection,
}

impl SearchIndex {
    pub fn new() -> Result<Self> {
        let connection = Connection::open_in_memory()?;
        connection.execute_batch(
            "CREATE VIRTUAL TABLE entries USING fts5(source UNINDEXED,
            conversation_id UNINDEXED, entry_id UNINDEXED, agent_id UNINDEXED,
            timestamp UNINDEXED, entry_type UNINDEXED, text, tokenize='unicode61');",
        )?;
        Ok(Self { connection })
    }

    pub fn replace(&mut self, source: &str, entries: &[SearchEntry]) -> Result<()> {
        let tx = self.connection.transaction()?;
        tx.execute("DELETE FROM entries WHERE source=?1", [source])?;
        {
            let mut insert = tx.prepare("INSERT INTO entries VALUES (?1,?2,?3,?4,?5,?6,?7)")?;
            for entry in entries {
                insert.execute(params![
                    source,
                    entry.conversation_id,
                    entry.entry_id,
                    entry.agent_id,
                    entry.timestamp,
                    entry.entry_type,
                    entry.text
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn remove(&mut self, source: &str) -> Result<()> {
        self.connection
            .execute("DELETE FROM entries WHERE source=?1", [source])?;
        Ok(())
    }

    /// Literal Unicode word phrase; FTS operators in a user's text never execute.
    /// `before` is the previous page's last (timestamp, conversation, entry) tuple.
    pub fn search(
        &self,
        text: &str,
        agent: Option<&str>,
        since: Option<&str>,
        before: Option<(&str, &str, &str)>,
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        ensure!(
            !text.trim().is_empty() && text.len() <= 512 && text.chars().any(char::is_alphanumeric),
            "search text needs 1 through 512 bytes and a word"
        );
        ensure!(
            (1..=201).contains(&limit),
            "search limit must be 1 through 201"
        );
        let phrase = format!("\"{}\"", text.replace('"', "\"\""));
        let (stamp, conversation, entry) = before.unwrap_or(("", "", ""));
        let mut statement = self.connection.prepare(
            "SELECT conversation_id, entry_id, agent_id,
            timestamp, entry_type, snippet(entries, 6, '', '', ' … ', 48)
            FROM entries WHERE entries MATCH ?1 AND (?2 IS NULL OR agent_id=?2)
            AND (?3 IS NULL OR timestamp>=?3)
            AND (?4='' OR (timestamp, conversation_id, entry_id)<(?4,?5,?6))
            ORDER BY timestamp DESC, conversation_id DESC, entry_id DESC LIMIT ?7",
        )?;
        let rows = statement.query_map(
            params![phrase, agent, since, stamp, conversation, entry, limit],
            |row| {
                let excerpt: String = row.get(5)?;
                Ok(SearchHit {
                    conversation_id: row.get(0)?,
                    entry_id: row.get(1)?,
                    agent_id: row.get(2)?,
                    timestamp: row.get(3)?,
                    entry_type: row.get(4)?,
                    excerpt: excerpt.chars().take(512).collect(),
                })
            },
        )?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn entry(id: &str, time: &str, text: &str) -> SearchEntry {
        SearchEntry {
            conversation_id: "session/test".into(),
            entry_id: id.into(),
            agent_id: Some("agent/test".into()),
            timestamp: time.into(),
            entry_type: "content".into(),
            text: text.into(),
        }
    }
    #[test]
    fn literal_phrase_paging_filters_and_replacements() {
        let mut index = SearchIndex::new().unwrap();
        index
            .replace(
                "test",
                &[
                    entry("old", "2026-10-01", "Hello café"),
                    entry("new", "2026-10-02", "hello café again"),
                    entry("other", "2026-10-03", "hello world"),
                ],
            )
            .unwrap();
        let hits = index.search("HELLO café", None, None, None, 1).unwrap();
        assert_eq!(hits[0].entry_id, "new");
        let last = &hits[0];
        let older = index
            .search(
                "hello café",
                None,
                None,
                Some((&last.timestamp, &last.conversation_id, &last.entry_id)),
                200,
            )
            .unwrap();
        assert_eq!(older[0].entry_id, "old");
        assert!(
            index
                .search("hello café", Some("agent/else"), None, None, 200)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            index
                .search("hello", None, Some("2026-10-02"), None, 200)
                .unwrap()
                .len(),
            2
        );
        assert!(
            index
                .search("hello OR world", None, None, None, 200)
                .unwrap()
                .is_empty()
        );
        index
            .replace("test", &[entry("new", "2026-10-02", "revised")])
            .unwrap();
        assert!(
            index
                .search("café", None, None, None, 200)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            index
                .search("revised", None, None, None, 200)
                .unwrap()
                .len(),
            1
        );
        index.remove("test").unwrap();
        assert!(
            index
                .search("revised", None, None, None, 200)
                .unwrap()
                .is_empty()
        );
    }
}
