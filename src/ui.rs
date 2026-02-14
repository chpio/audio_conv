use crate::ConversionArgs;
use anyhow::{Context, Result};
use futures::Future;
use std::{
	borrow::Cow, cell::RefCell, collections::HashMap, io, mem, path::PathBuf, rc::Rc,
	time::Duration,
};
use tokio::{task, time::interval};
use tui::{Terminal, backend::CrosstermBackend};

pub const UPDATE_INTERVAL_MILLIS: u64 = 100;

#[derive(Debug)]
pub enum Msg {
	Init { task_len: usize, log_path: PathBuf },
	Exit,
	TaskStart { id: usize, args: ConversionArgs },
	TaskEnd { id: usize },
	TaskProgress { id: usize, ratio: f64 },
	TaskError { id: usize },
}

#[derive(Debug, Clone)]
pub struct MsgQueue {
	inner: Rc<RefCell<Vec<Msg>>>,
}

impl MsgQueue {
	fn new() -> MsgQueue {
		MsgQueue {
			inner: Rc::new(RefCell::new(Vec::new())),
		}
	}

	pub fn push(&self, msg: Msg) {
		self.inner.borrow_mut().push(msg);
	}

	fn swap_inner(&self, other: &mut Vec<Msg>) {
		let mut inner = self.inner.borrow_mut();
		mem::swap(&mut *inner, other)
	}
}

struct State {
	terminal: Terminal<CrosstermBackend<io::Stdout>>,
	log_path: Option<PathBuf>,
	task_len: Option<usize>,
	ended_tasks: usize,
	running_tasks: HashMap<usize, Task>,
	has_rendered: bool,
	has_errored: bool,
}

impl State {
	fn new() -> Result<State> {
		let terminal = Terminal::new(CrosstermBackend::new(io::stdout()))
			.context("Unable to create ui terminal")?;

		Ok(State {
			terminal,
			log_path: None,
			task_len: None,
			ended_tasks: 0,
			running_tasks: HashMap::new(),
			has_rendered: false,
			has_errored: false,
		})
	}

	fn process_msg(&mut self, msg: Msg) -> Result<bool> {
		match msg {
			Msg::Init { task_len, log_path } => {
				self.task_len = Some(task_len);
				self.log_path = Some(log_path);
			}
			Msg::Exit => return Ok(false),
			Msg::TaskStart { id, args } => {
				self.running_tasks.insert(
					id,
					Task {
						id,
						ratio: None,
						args,
					},
				);
			}
			Msg::TaskEnd { id } => {
				self.running_tasks
					.remove(&id)
					.context("Unable to remove finished task; could't find task")?;
				self.ended_tasks += 1;
			}
			Msg::TaskProgress { id, ratio } => {
				let task = self
					.running_tasks
					.get_mut(&id)
					.context("Unable to update task progress; could't find task")?;
				task.ratio = Some(ratio);
			}
			Msg::TaskError { id } => {
				// TODO
				self.running_tasks
					.remove(&id)
					.context("Unable to remove errored task; could't find task")?;
				self.ended_tasks += 1;
				self.has_errored = true;
			}
		}

		Ok(true)
	}

	fn render(&mut self) -> Result<()> {
		use tui::{
			layout::{Constraint, Direction, Layout, Rect},
			style::{Color, Modifier, Style},
			text::Text,
			widgets::{Block, Borders, Gauge, Paragraph},
		};

		let task_len = if let Some(task_len) = self.task_len {
			task_len
		} else {
			return Ok(());
		};

		if task_len == 0 {
			return Ok(());
		}

		let tasks_ended = self.ended_tasks;

		let mut running_tasks: Vec<_> = self.running_tasks.values().cloned().collect();

		running_tasks.sort_by_key(|task| task.id);

		if !self.has_rendered {
			self.terminal.clear().context("Clearing ui failed")?;
			self.has_rendered = true;
		}

		let error_text = match self.has_errored {
			true => {
				let text: Cow<'static, str> = self
					.log_path
					.as_ref()
					.map(|lp| {
						let text = format!("Error(s) occurred and were logged to {}", lp.display());
						Cow::Owned(text)
					})
					.unwrap_or_else(|| Cow::Borrowed("Error(s) occurred"));
				Some(text)
			}
			false => None,
		};

		self.terminal
			.draw(|f| {
				let chunks = Layout::default()
					.direction(Direction::Vertical)
					.margin(1)
					.constraints([Constraint::Percentage(90), Constraint::Percentage(10)].as_ref())
					.split(f.size());

				let mut task_rect = chunks[0];

				if error_text.is_some() {
					task_rect.height -= 3;
				}

				for (row, task) in running_tasks
					.into_iter()
					.take(task_rect.height as usize / 2)
					.enumerate()
				{
					f.render_widget(
						Gauge::default()
							.label(task.args.rel_from_path.to_string_lossy().as_ref())
							.gauge_style(
								Style::default()
									.fg(Color::White)
									.bg(Color::Black)
									.add_modifier(Modifier::ITALIC),
							)
							.ratio(task.ratio.unwrap_or(0.0)),
						Rect::new(
							task_rect.x,
							task_rect.y + row as u16 * 2,
							task_rect.width,
							1,
						),
					);
				}

				if let Some(error_text) = error_text {
					f.render_widget(
						Paragraph::new(Text::raw(error_text)).style(
							Style::default()
								.fg(Color::Red)
								.bg(Color::Black)
								.add_modifier(Modifier::BOLD),
						),
						Rect::new(task_rect.x, task_rect.height + 1, task_rect.width, 2),
					);
				}

				f.render_widget(
					Gauge::default()
						.block(
							Block::default()
								.borders(Borders::ALL)
								.title("Overall Progress"),
						)
						.gauge_style(
							Style::default()
								.fg(Color::White)
								.bg(Color::Black)
								.add_modifier(Modifier::ITALIC),
						)
						.ratio(tasks_ended as f64 / task_len as f64),
					chunks[1],
				);
			})
			.context("Rendering ui failed")?;

		Ok(())
	}
}

#[derive(Debug, Clone)]
struct Task {
	id: usize,
	ratio: Option<f64>,
	args: ConversionArgs,
}

pub fn init() -> (MsgQueue, impl Future<Output = Result<()>>) {
	let queue = MsgQueue::new();

	let queue_clone = queue.clone();
	let fut = async move {
		let mut interval = interval(Duration::from_millis(UPDATE_INTERVAL_MILLIS));
		let mut wrapped = Some((Vec::new(), State::new()?));

		loop {
			interval.tick().await;

			let (mut current_queue, mut state) = wrapped.take().context("`wrapped` is None")?;

			queue_clone.swap_inner(&mut current_queue);

			let render_res = task::spawn_blocking(move || -> Result<_> {
				let mut exit = false;
				for msg in current_queue.drain(..) {
					if !state.process_msg(msg)? {
						exit = true;
					}
				}

				state.render()?;

				if exit {
					Ok(None)
				} else {
					Ok(Some((current_queue, state)))
				}
			})
			.await
			.context("Ui update task failed")?
			.context("Ui update failed")?;

			match render_res {
				Some(s) => wrapped = Some(s),
				None => break,
			}
		}

		Result::<_>::Ok(())
	};

	(queue, fut)
}
