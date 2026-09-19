//! World model: terrain, robots, channels, and the shared execution context.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;

use slew::NativeWait;

use crate::config::{MAP_H, MAP_W, MAX_LOG, ROBOTS, SHARED_CHANNEL};

#[derive(Clone, Copy, PartialEq, Eq, Debug, strum::EnumString, strum::IntoStaticStr)]
pub(crate) enum Facing {
    // `to_string` pins the canonical lower-case name returned by `name()` and
    // is also accepted by `from_name`; `serialize` entries add aliases.
    #[strum(to_string = "north", serialize = "North", serialize = "N")]
    North,
    #[strum(to_string = "east", serialize = "East", serialize = "E")]
    East,
    #[strum(to_string = "south", serialize = "South", serialize = "S")]
    South,
    #[strum(to_string = "west", serialize = "West", serialize = "W")]
    West,
}

impl Facing {
    pub(crate) fn from_name(name: &[u8]) -> Option<Self> {
        std::str::from_utf8(name).ok()?.parse().ok()
    }

    pub(crate) fn name(self) -> &'static str {
        self.into()
    }

    pub(crate) fn delta(self) -> (i32, i32) {
        match self {
            Self::North => (0, -1),
            Self::East => (1, 0),
            Self::South => (0, 1),
            Self::West => (-1, 0),
        }
    }

    pub(crate) fn glyph(self) -> char {
        match self {
            Self::North => '^',
            Self::East => '>',
            Self::South => 'v',
            Self::West => '<',
        }
    }

    pub(crate) fn left(self) -> Self {
        match self {
            Self::North => Self::West,
            Self::East => Self::North,
            Self::South => Self::East,
            Self::West => Self::South,
        }
    }

    pub(crate) fn right(self) -> Self {
        match self {
            Self::North => Self::East,
            Self::East => Self::South,
            Self::South => Self::West,
            Self::West => Self::North,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Cell {
    Empty,
    Wall,
    Ore(u32),
}

#[derive(Clone, Copy)]
pub(crate) enum JobKind {
    Move(i32, i32),
    Mine,
}

#[derive(Clone, Copy)]
pub(crate) struct Job {
    pub(crate) kind: JobKind,
    pub(crate) remaining: u32,
}

pub(crate) struct RobotState {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) facing: Facing,
    pub(crate) carried: u32,
    pub(crate) job: Option<Job>,
}

/// A channel payload. Cross-`Lua` messages cannot be raw Lua values, so they
/// are converted to this scalar representation at the native boundary.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Msg {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Vec<u8>),
}

pub(crate) struct Channel {
    pub(crate) cap: usize,
    pub(crate) items: VecDeque<Msg>,
}

pub(crate) struct World {
    pub(crate) w: i32,
    pub(crate) h: i32,
    pub(crate) cells: Vec<Cell>,
    pub(crate) robots: Vec<RobotState>,
    pub(crate) channels: HashMap<i64, Channel>,
    /// Authorized channel ids, indexed by robot.
    pub(crate) grants: Vec<HashSet<i64>>,
    /// Pending lifecycle actions requested by robot programs.
    pub(crate) requests: Vec<Option<Request>>,
    pub(crate) log: Rc<RefCell<Vec<String>>>,
}

impl World {
    pub(crate) fn generate() -> Self {
        let (w, h) = (MAP_W, MAP_H);
        let idx = |x: i32, y: i32| (y * w + x) as usize;
        let mut cells = vec![Cell::Empty; (w * h) as usize];
        for x in 0..w {
            cells[idx(x, 0)] = Cell::Wall;
            cells[idx(x, h - 1)] = Cell::Wall;
        }
        for y in 0..h {
            cells[idx(0, y)] = Cell::Wall;
            cells[idx(w - 1, y)] = Cell::Wall;
        }
        // Clear a home pad in the top-left.
        for y in 1..6 {
            for x in 1..6 {
                cells[idx(x, y)] = Cell::Empty;
            }
        }

        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut rnd = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..(w * h / 10) {
            let x = 1 + (rnd() % (w as u64 - 2)) as i32;
            let y = 6 + (rnd() % (h as u64 - 7)) as i32;
            if x > 0 && y > 0 && x < w - 1 && y < h - 1 {
                cells[idx(x, y)] = Cell::Wall;
            }
        }
        for _ in 0..(w * h / 20) {
            let x = 1 + (rnd() % (w as u64 - 2)) as i32;
            let y = 6 + (rnd() % (h as u64 - 7)) as i32;
            if cells[idx(x, y)] == Cell::Empty {
                cells[idx(x, y)] = Cell::Ore(3 + (rnd() % 5) as u32);
            }
        }

        let robots = (0..ROBOTS)
            .map(|i| RobotState {
                x: 1 + i as i32,
                y: 1,
                facing: Facing::East,
                carried: 0,
                job: None,
            })
            .collect();
        let grants = (0..ROBOTS)
            .map(|i| {
                let mut s = HashSet::new();
                s.insert(i as i64);
                s.insert(SHARED_CHANNEL);
                s
            })
            .collect();

        Self {
            w,
            h,
            cells,
            robots,
            channels: HashMap::new(),
            grants,
            requests: (0..ROBOTS).map(|_| None).collect(),
            log: Rc::new(RefCell::new(Vec::new())),
        }
    }

    /// Appends a robot at a free home-pad tile and returns its id.
    pub(crate) fn add_robot(&mut self) -> usize {
        let id = self.robots.len();
        let mut spot = None;
        'search: for y in 1..6 {
            for x in 1..6 {
                if self.at(x, y) != Some(Cell::Wall) && !self.occupied_by_other(usize::MAX, x, y) {
                    spot = Some((x, y));
                    break 'search;
                }
            }
        }
        let (x, y) = spot.unwrap_or((1, 1));
        self.robots.push(RobotState {
            x,
            y,
            facing: Facing::East,
            carried: 0,
            job: None,
        });
        let mut grant = HashSet::new();
        grant.insert(id as i64);
        grant.insert(SHARED_CHANNEL);
        self.grants.push(grant);
        self.requests.push(None);
        id
    }

    pub(crate) fn push_log(&self, rid: usize, text: impl AsRef<str>) {
        self.log_line(format!("[R{rid}] {}", text.as_ref()));
    }

    pub(crate) fn log_line(&self, text: impl AsRef<str>) {
        let mut log = self.log.borrow_mut();
        log.push(text.as_ref().to_string());
        if log.len() > MAX_LOG {
            let excess = log.len() - MAX_LOG;
            log.drain(..excess);
        }
    }

    pub(crate) fn at(&self, x: i32, y: i32) -> Option<Cell> {
        if x < 0 || y < 0 || x >= self.w || y >= self.h {
            None
        } else {
            Some(self.cells[(y * self.w + x) as usize])
        }
    }

    pub(crate) fn occupied_by_other(&self, rid: usize, x: i32, y: i32) -> bool {
        self.robots
            .iter()
            .enumerate()
            .any(|(j, r)| j != rid && r.x == x && r.y == y)
    }

    /// Advances one game tick, completing any elapsed jobs.
    pub(crate) fn tick(&mut self) {
        let World {
            w, cells, robots, ..
        } = self;
        let w = *w;
        for r in robots.iter_mut() {
            let Some(job) = &mut r.job else { continue };
            if job.remaining > 0 {
                job.remaining -= 1;
            }
            if job.remaining > 0 {
                continue;
            }
            let job = r.job.take().expect("job checked above");
            match job.kind {
                JobKind::Move(nx, ny) => {
                    r.x = nx;
                    r.y = ny;
                }
                JobKind::Mine => {
                    let idx = (r.y * w + r.x) as usize;
                    if let Cell::Ore(n) = cells[idx] {
                        if n > 0 {
                            cells[idx] = if n == 1 {
                                Cell::Empty
                            } else {
                                Cell::Ore(n - 1)
                            };
                            r.carried += 1;
                        }
                    }
                }
            }
        }
    }
}

/// A lifecycle action a robot program has requested from the host.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Request {
    Shutdown,
    Reboot,
}

/// What a parked native wait is waiting for. The kind is encoded in the
/// [`NativeWait`] token (see [`WaitKind::token`]) so a wait stays completable
/// when another execution adopts the parked coroutine.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaitKind {
    /// Robot `usize`'s current job has finished (`robot.wait()`).
    ActionDone(usize),
    /// The channel has at least one message (`ch.recv`).
    ChannelNonEmpty(i64),
    /// The channel has room for another message (`ch.send`).
    ChannelRoom(i64),
}

impl WaitKind {
    /// The kind occupies the top byte of the token; its payload the rest.
    const KIND_SHIFT: u32 = 56;
    const PAYLOAD_MASK: u64 = 0x00FF_FFFF_FFFF_FFFF;

    /// The kind's numeric tag and its payload (robot id or channel id).
    fn parts(self) -> (u64, i64) {
        match self {
            WaitKind::ActionDone(rid) => (0, rid as i64),
            WaitKind::ChannelNonEmpty(chan) => (1, chan),
            WaitKind::ChannelRoom(chan) => (2, chan),
        }
    }

    /// The token naming this wait. The payload must be non-negative and fit in
    /// 56 bits (channel ids are, and robot ids are tiny).
    pub(crate) fn token(self) -> NativeWait {
        let (kind, payload) = self.parts();
        debug_assert!(
            payload >= 0 && payload as u64 <= Self::PAYLOAD_MASK,
            "wait payload {payload} does not fit in the token"
        );
        NativeWait((kind << Self::KIND_SHIFT) | (payload as u64 & Self::PAYLOAD_MASK))
    }

    /// The wait a token names, or `None` for a token this host did not mint.
    pub(crate) fn from_token(token: NativeWait) -> Option<Self> {
        let payload = (token.0 & Self::PAYLOAD_MASK) as i64;
        match token.0 >> Self::KIND_SHIFT {
            0 => Some(WaitKind::ActionDone(payload as usize)),
            1 => Some(WaitKind::ChannelNonEmpty(payload)),
            2 => Some(WaitKind::ChannelRoom(payload)),
            _ => None,
        }
    }

    /// Whether the world condition this wait is waiting for currently holds.
    /// The wait itself names the robot or channel it concerns.
    pub(crate) fn is_ready(self, world: &World) -> bool {
        match self {
            WaitKind::ActionDone(rid) => world.robots[rid].job.is_none(),
            WaitKind::ChannelNonEmpty(chan) => world
                .channels
                .get(&chan)
                .is_some_and(|c| !c.items.is_empty()),
            WaitKind::ChannelRoom(chan) => world
                .channels
                .get(&chan)
                .is_none_or(|c| c.items.len() < c.cap),
        }
    }
}

/// Per-execution context. All of a robot's executions share the `Rc`s.
#[derive(Clone)]
pub(crate) struct Ctx {
    pub(crate) robot: usize,
    pub(crate) world: Rc<RefCell<World>>,
}
