use super::*;
use std::sync::mpsc;
use std::time::Duration;

fn writer() -> WriterConnection {
    let connection = Connection::open_in_memory().unwrap();
    connection.execute_batch("CREATE TABLE claims(store_index INTEGER PRIMARY KEY AUTOINCREMENT); CREATE TABLE values_(id INTEGER PRIMARY KEY)").unwrap();
    WriterConnection::new(connection, Arc::new(AtomicU64::new(0)))
}

struct Loan {
    lent: Receiver<Connection>,
    returned: mpsc::SyncSender<Connection>,
}

fn loan() -> (WriterJob, Loan) {
    let (lent, receives) = mpsc::sync_channel(1);
    let (gives, returned) = mpsc::sync_channel(1);
    (
        WriterJob::Lend { lent, returned },
        Loan {
            lent: receives,
            returned: gives,
        },
    )
}

impl Loan {
    fn acquire(&self) -> Connection {
        self.lent.recv_timeout(Duration::from_secs(10)).unwrap()
    }
    fn release(&self, connection: Connection) {
        self.returned.send(connection).unwrap();
    }
}

fn foreground(writer: &WriterConnection) -> Loan {
    let (job, loan) = loan();
    writer.send(job);
    loan
}

fn background(writer: &WriterConnection) -> Loan {
    let (job, loan) = loan();
    writer.enqueue_background(job);
    loan
}

fn selected_loans(writer: &WriterConnection, count: usize) -> Vec<Loan> {
    (0..count).map(|_| foreground(writer)).collect()
}

#[test]
fn foreground_fifo_overtakes_earlier_background_without_preempting_holder() {
    let writer = writer();
    let held = writer.write();
    let back = [background(&writer), background(&writer)];
    let front = selected_loans(&writer, 3);
    assert!(front[0].lent.try_recv().is_err());
    assert!(back[0].lent.try_recv().is_err());
    drop(held);
    for next in &front {
        let connection = next.acquire();
        assert!(back[0].lent.try_recv().is_err());
        next.release(connection);
    }
    let connection = back[0].acquire();
    assert!(back[1].lent.try_recv().is_err());
    back[0].release(connection);
    back[1].release(back[1].acquire());
}

#[test]
fn background_progress_is_bounded_by_foreground_turns_under_continuous_backlog() {
    let writer = writer();
    let held = writer.write();
    let back = [background(&writer), background(&writer)];
    let front = selected_loans(&writer, FOREGROUND_TURNS * 3);
    drop(held);
    for (i, group) in front.chunks(FOREGROUND_TURNS).enumerate() {
        for next in group {
            next.release(next.acquire());
        }
        if i < back.len() {
            let connection = back[i].acquire();
            if let Some(later) = front.get((i + 1) * FOREGROUND_TURNS) {
                assert!(later.lent.try_recv().is_err());
            }
            back[i].release(connection);
        }
    }
}

#[test]
fn abandoned_background_request_returns_connection_and_does_not_block_foreground() {
    let writer = writer();
    let held = writer.write();
    drop(background(&writer));
    let front = foreground(&writer);
    let back = background(&writer);
    drop(held);
    front.release(front.acquire());
    back.release(back.acquire());
    writer
        .batched(|tx| tx.execute("INSERT INTO values_ VALUES(1)", []))
        .unwrap()
        .unwrap();
}

#[test]
fn background_guard_retains_prepare_finalize_rollback_and_return_observer_order() {
    let writer = writer();
    writer
        .install_transaction_hooks(
            |tx| {
                tx.execute("INSERT INTO values_ VALUES(1)", [])?;
                Ok(())
            },
            |tx| {
                tx.execute("INSERT INTO values_ VALUES(2)", [])?;
                Ok(())
            },
        )
        .unwrap();
    let (sent, observed) = mpsc::channel();
    let _observer = writer.observe_commits(move |connection| {
        let rows: u64 = connection
            .query_row("SELECT COUNT(*) FROM values_", [], |r| r.get(0))
            .unwrap();
        sent.send(rows).unwrap();
    });
    {
        let mut connection = writer.write_background();
        let tx = connection.transaction().unwrap();
        tx.execute("INSERT INTO values_ VALUES(3)", []).unwrap();
        tx.rollback().unwrap();
    }
    assert_eq!(observed.recv().unwrap(), 0);
    {
        let mut connection = writer.write_background();
        let tx = connection.transaction().unwrap();
        tx.execute("INSERT INTO values_ VALUES(3)", []).unwrap();
        tx.commit().unwrap();
        assert!(
            observed.try_recv().is_err(),
            "notice must wait for full guard return"
        );
    }
    assert_eq!(observed.recv().unwrap(), 3);
}

#[test]
fn background_finalizer_refusal_rolls_back_before_next_foreground_work() {
    let writer = writer();
    writer
        .install_transaction_hooks(
            |_| Ok(()),
            |tx| {
                tx.execute("INSERT INTO values_ VALUES(2)", [])?;
                anyhow::bail!("refused derived publication")
            },
        )
        .unwrap();
    let mut guard = writer.write_background();
    let tx = guard.transaction().unwrap();
    tx.execute("INSERT INTO values_ VALUES(1)", []).unwrap();
    assert!(tx.commit().is_err());
    drop(guard);
    assert_eq!(
        writer
            .write()
            .query_row("SELECT COUNT(*) FROM values_", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn hook_configuration_fence_drains_prior_background_but_not_later_background() {
    // Drive the exact shared admission scheduler without wall clocks or thread races.
    let (sent, received) = mpsc::channel();
    let lifetime = Arc::new(BackgroundAdmission::default());
    let queue = lifetime
        .queue
        .get_or_init(|| Arc::new(BackgroundQueue::default()));
    let (old, old_loan) = loan();
    assert!(queue.push(old));
    sent.send(WriterJob::BackgroundReady(queue.clone()))
        .unwrap();
    let through = queue.watermark();
    let (job, fence) = loan();
    let WriterJob::Lend { lent, returned } = job else {
        unreachable!()
    };
    sent.send(WriterJob::FenceLend {
        through,
        lent,
        returned,
    })
    .unwrap();
    let (later, later_loan) = loan();
    queue.push(later);
    let mut admission = Admission::new(received, lifetime.clone());
    let first = admission.next().unwrap();
    let WriterJob::Lend { lent, .. } = first else {
        panic!("prior background must drain")
    };
    lent.send(Connection::open_in_memory().unwrap()).unwrap();
    assert!(old_loan.lent.try_recv().is_ok());
    assert!(fence.lent.try_recv().is_err());
    assert!(later_loan.lent.try_recv().is_err());
    assert!(matches!(
        admission.next(),
        Some(WriterJob::FenceLend { .. })
    ));
    assert!(matches!(admission.next(), Some(WriterJob::Lend { .. })));
}

#[test]
fn stopped_writer_disconnects_background_requests_even_before_notification_consumption() {
    let (sent, received) = mpsc::channel();
    let lifetime = Arc::new(BackgroundAdmission::default());
    let mut admission = Admission::new(received, lifetime.clone());
    let queue = lifetime
        .queue
        .get_or_init(|| Arc::new(BackgroundQueue::default()));
    let (job, loan) = loan();
    queue.push(job);
    sent.send(WriterJob::BackgroundReady(queue.clone()))
        .unwrap();
    drop(admission.next().unwrap()); // loan selected but its caller never receives a connection
    let (job, pending) = self::loan();
    queue.push(job);
    drop(admission);
    assert!(matches!(
        loan.lent.try_recv(),
        Err(TryRecvError::Disconnected)
    ));
    assert!(matches!(
        pending.lent.try_recv(),
        Err(TryRecvError::Disconnected)
    ));
}

#[test]
fn foreground_discovery_work_is_independent_of_background_backlog() {
    for size in [12, 12_000] {
        let (sent, received) = mpsc::channel();
        let lifetime = Arc::new(BackgroundAdmission::default());
        let queue = lifetime
            .queue
            .get_or_init(|| Arc::new(BackgroundQueue::default()));
        let mut borrowers = Vec::new();
        for _ in 0..size {
            let (job, loan) = loan();
            queue.push(job);
            borrowers.push(loan);
        }
        sent.send(WriterJob::BackgroundReady(queue.clone()))
            .unwrap();
        let (front, front_loan) = loan();
        sent.send(front).unwrap();
        let mut admission = Admission::new(received, lifetime);
        let WriterJob::Lend { lent, .. } = admission.next().unwrap() else {
            unreachable!()
        };
        lent.send(Connection::open_in_memory().unwrap()).unwrap();
        assert!(front_loan.lent.try_recv().is_ok());
        assert!(borrowers[0].lent.try_recv().is_err());
        assert_eq!(
            admission.inspected, 2,
            "one notification and one foreground head; no scan"
        );
    }
}

#[test]
fn foreground_only_fifo_and_batched_durable_answers_are_unchanged() {
    let writer = writer();
    let held = writer.write();
    let first = foreground(&writer);
    let (done, answer) = mpsc::sync_channel(1);
    writer.send(WriterJob::Batched {
        run: Box::new(|tx| {
            tx.execute("INSERT INTO values_ VALUES(1)", []).unwrap();
            true
        }),
        profile: None,
        wait: None,
        done,
    });
    let last = foreground(&writer);
    drop(held);
    let connection = first.acquire();
    assert!(answer.try_recv().is_err());
    first.release(connection);
    answer
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    let connection = last.acquire();
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM values_", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        1
    );
    last.release(connection);
}

#[test]
fn actual_worker_failure_disconnects_queued_background_and_foreground_borrowers() {
    let writer = writer();
    let active = foreground(&writer);
    let connection = active.acquire();
    let pending = background(&writer);
    let front = foreground(&writer);
    drop(connection);
    drop(active);
    assert!(matches!(
        pending.lent.recv_timeout(Duration::from_secs(10)),
        Err(mpsc::RecvTimeoutError::Disconnected)
    ));
    assert!(matches!(
        front.lent.recv_timeout(Duration::from_secs(10)),
        Err(mpsc::RecvTimeoutError::Disconnected)
    ));
}

#[test]
fn real_hook_installation_barrier_waits_for_prior_background_return() {
    let writer = Arc::new(writer());
    let held = writer.write();
    let old = background(&writer);
    let (proxy, incoming) = mpsc::channel();
    let original = writer.jobs.lock().unwrap().replace(proxy).unwrap();
    let (done, installed) = mpsc::sync_channel(1);
    let installing = writer.clone();
    let thread = std::thread::spawn(move || {
        let result = installing.install_transaction_hooks(
            |tx| {
                tx.execute("INSERT INTO values_ VALUES(1)", [])?;
                Ok(())
            },
            |tx| {
                tx.execute("INSERT INTO values_ VALUES(2)", [])?;
                Ok(())
            },
        );
        done.send(result).unwrap();
    });
    let barrier = incoming.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(matches!(barrier, WriterJob::FenceLend { .. }));
    original.send(barrier).unwrap();
    *writer.jobs.lock().unwrap() = Some(original);
    let later = background(&writer);
    drop(held);
    let connection = old.acquire();
    assert!(installed.try_recv().is_err());
    old.release(connection);
    installed
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    let connection = later.acquire();
    // This raw borrowed loan is behind the configuration barrier, so manually execute a
    // managed guard loan next to prove both installed phases apply together.
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM values_", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        0
    );
    later.release(connection);
    thread.join().unwrap();
    let mut guard = writer.write_background();
    guard.transaction().unwrap().commit().unwrap();
    drop(guard);
    assert_eq!(
        writer
            .write()
            .query_row("SELECT COUNT(*) FROM values_", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        2
    );
}
