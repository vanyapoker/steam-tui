use super::model::Model;
use std::collections::BTreeMap;
use steam_core::updates::{Job, JobState, Queue};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Kind {
    Content,
    Shaders,
    Dependency,
}
impl Kind {
    pub fn of(job: &Job) -> Self {
        if job.intent.shader {
            Self::Shaders
        } else if job.intent.dependency.is_some() {
            Self::Dependency
        } else {
            Self::Content
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Content => "Content",
            Self::Shaders => "Shaders",
            Self::Dependency => "Dependencies",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Key {
    Job(u64),
    History(u32, Kind),
}
#[derive(Clone, Debug)]
pub(super) struct Entry {
    pub key: Key,
    pub runs: Vec<u64>,
}
impl Model {
    pub fn download_entries(&self) -> Vec<Entry> {
        let mut current = Vec::new();
        let mut history: BTreeMap<(u32, Kind), Vec<&Job>> = BTreeMap::new();
        for job in &self.queue_jobs {
            let kind = Kind::of(job);
            if self.queue_filter.is_some_and(|filter| filter != kind) {
                continue;
            }
            if matches!(job.state, JobState::Complete | JobState::Cancelled) {
                history
                    .entry((job.intent.app_id, kind))
                    .or_default()
                    .push(job);
            } else {
                current.push(job);
            }
        }
        current.sort_by_key(|job| {
            (
                match job.state {
                    JobState::Running => 0,
                    JobState::Failed => 1,
                    JobState::Paused => 2,
                    _ => 3,
                },
                std::cmp::Reverse(job.foreground),
                std::cmp::Reverse(job.priority),
                job.id,
            )
        });
        let mut entries: Vec<_> = current
            .into_iter()
            .map(|job| Entry {
                key: Key::Job(job.id),
                runs: vec![job.id],
            })
            .collect();
        let mut groups: Vec<_> = history.into_iter().collect();
        for (_, runs) in &mut groups {
            runs.sort_by_key(|j| std::cmp::Reverse((j.finished_at, j.id)));
        }
        groups.sort_by_key(|(_, runs)| std::cmp::Reverse((runs[0].finished_at, runs[0].id)));
        entries.extend(groups.into_iter().map(|((app, kind), runs)| Entry {
            key: Key::History(app, kind),
            runs: runs.into_iter().map(|j| j.id).collect(),
        }));
        entries
    }
    pub fn selected_download(&self) -> Option<&Job> {
        let entries = self.download_entries();
        let entry = entries.get(self.queue_selected)?;
        let id = self
            .queue_run
            .filter(|id| entry.runs.contains(id))
            .unwrap_or(entry.runs[0]);
        self.queue_jobs.iter().find(|j| j.id == id)
    }
    pub fn refresh_downloads(&mut self, queue: &Queue) {
        let key = self
            .download_entries()
            .get(self.queue_selected)
            .map(|e| e.key.clone());
        let selected_id = self.selected_download().map(|job| job.id);
        let result = queue
            .jobs()
            .and_then(|jobs| queue.settings().map(|settings| (jobs, settings)));
        match result {
            Ok((jobs, settings)) => {
                self.queue_load_error = None;
                self.queue_jobs = jobs;
                self.queue_settings = settings;
                let entries = self.download_entries();
                let position = selected_id
                    .and_then(|id| entries.iter().position(|e| e.key == Key::Job(id)))
                    .or_else(|| key.and_then(|key| entries.iter().position(|e| e.key == key)))
                    .or_else(|| {
                        selected_id.and_then(|id| entries.iter().position(|e| e.runs.contains(&id)))
                    });
                self.queue_selected = position
                    .unwrap_or(self.queue_selected)
                    .min(entries.len().saturating_sub(1));
                if let Some(id) = selected_id.filter(|id| {
                    entries
                        .get(self.queue_selected)
                        .is_some_and(|e| e.runs.contains(id))
                }) && (self.queue_run.is_some()
                    || entries
                        .get(self.queue_selected)
                        .is_some_and(|e| e.runs[0] != id))
                {
                    self.queue_run = Some(id);
                }
            }
            Err(error) => self.queue_load_error = Some(error.to_string()),
        }
    }
    pub fn download_name(&self, app: u32) -> String {
        self.games
            .iter()
            .find(|g| g.app_id == app)
            .map(|g| g.name.clone())
            .filter(|n| !n.trim().is_empty())
            .or_else(|| self.queue_names.get(&app).cloned())
            .unwrap_or_else(|| format!("App {app}"))
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::super::model::Action;
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    pub(crate) fn job(id: u64, app: u32, state: JobState, finished_at: Option<i64>) -> Job {
        Job {
            id,
            intent: steam_core::updates::Intent {
                app_id: app,
                dependency: None,
                shader: false,
                revision: None,
                steam_root: "/games".into(),
                destination: "/games/library".into(),
            },
            state,
            foreground: false,
            priority: 0,
            error: None,
            created_at: None,
            started_at: None,
            finished_at,
        }
    }
    fn press(model: &mut Model, code: KeyCode) -> Action {
        model.key(KeyEvent::new(code, KeyModifiers::NONE))
    }
    #[test]
    fn grouping_order_filters_and_exact_history_actions() {
        let mut m = Model::new(true, None);
        m.queue_view = true;
        m.queue_jobs = vec![
            job(1, 10, JobState::Complete, None),
            job(2, 10, JobState::Cancelled, Some(100)),
            job(3, 11, JobState::Complete, None),
            job(4, 12, JobState::Failed, Some(90)),
            job(5, 13, JobState::Pending, None),
            job(6, 14, JobState::Running, None),
            job(7, 15, JobState::Paused, None),
            job(8, 16, JobState::Pending, None),
        ];
        m.queue_jobs[7].priority = 10;
        m.queue_jobs[4].foreground = true;
        assert_eq!(
            m.download_entries()
                .iter()
                .map(|e| e.runs[0])
                .collect::<Vec<_>>(),
            [6, 4, 7, 5, 8, 2, 3]
        );
        m.queue_selected = 5;
        assert_eq!(press(&mut m, KeyCode::Char('p')), Action::None);
        assert_eq!(
            press(&mut m, KeyCode::Char('r')),
            Action::QueueControl(2, JobState::Pending)
        );
        press(&mut m, KeyCode::Left);
        assert_eq!(m.selected_download().unwrap().id, 1);
        assert_eq!(press(&mut m, KeyCode::Char('r')), Action::None);
        assert_eq!(press(&mut m, KeyCode::Char('c')), Action::None);
        press(&mut m, KeyCode::Right);
        assert_eq!(m.selected_download().unwrap().id, 2);
        press(&mut m, KeyCode::Char('f'));
        assert_eq!(m.download_entries().len(), 7);
        press(&mut m, KeyCode::Char('f'));
        assert!(m.download_entries().is_empty());
        let mut shader = job(9, 10, JobState::Complete, Some(200));
        shader.intent.shader = true;
        m.queue_jobs.push(shader);
        assert_eq!(m.download_entries().len(), 1);
        let mut dependency = job(10, 10, JobState::Complete, Some(300));
        dependency.intent.dependency = Some(steam_core::updates::DependencyIntent {
            parent_app_id: 20,
            requirement: steam_core::launch::Dependency::Additional { app_id: 10 },
        });
        m.queue_jobs.push(dependency);
        press(&mut m, KeyCode::Char('f'));
        assert_eq!(m.download_entries()[0].runs, [10]);
        press(&mut m, KeyCode::Char('f'));
        assert_eq!(m.download_entries().len(), 9);
        assert_eq!(m.download_name(99), "App 99");
        m.queue_names.insert(99, "Cached name".into());
        assert_eq!(m.download_name(99), "Cached name");
    }
    #[test]
    fn refresh_preserves_group_and_browsed_run_when_new_work_arrives() {
        let root =
            std::env::temp_dir().join(format!("steam-tui-download-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let q = Queue::in_directory(&root, "selection").unwrap();
        let id = q
            .enqueue(job(1, 10, JobState::Pending, None).intent, false)
            .unwrap();
        q.control(id, JobState::Cancelled).unwrap();
        let mut m = Model::new(true, None);
        m.refresh_downloads(&q);
        m.queue_run = Some(id);
        q.enqueue(job(2, 20, JobState::Pending, None).intent, true)
            .unwrap();
        m.refresh_downloads(&q);
        assert_eq!(m.queue_selected, 1);
        assert_eq!(m.selected_download().unwrap().id, id);
        q.control(id, JobState::Pending).unwrap();
        m.refresh_downloads(&q);
        assert_eq!(m.selected_download().unwrap().id, id);
        q.control(id, JobState::Cancelled).unwrap();
        m.refresh_downloads(&q);
        assert_eq!(m.selected_download().unwrap().id, id);
        std::fs::write(root.join("steam-tui/updates-selection/jobs.json"), "broken").unwrap();
        m.refresh_downloads(&q);
        assert!(m.queue_load_error.is_some());
        assert_eq!(m.selected_download().unwrap().id, id);
        std::fs::remove_dir_all(root).unwrap();
    }
}
