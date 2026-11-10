//! Guests built into the binary, used by `bohr selftest`.
//!
//! They exercise the clock, randomness and thread creation without a program on disk or tracing
//! privileges. They are the same fixtures the test suite runs.

use bohr_backend_null::NullProgram;
use bohr_testkit::Fixture;

/// A guest available to `bohr selftest`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, clap::ValueEnum)]
pub(crate) enum Demo {
    /// Reads the clock and asks for randomness.
    ClockAndRandom,
    /// Two tasks interleaving, each reading the clock and asking for randomness.
    TwoTasks,
    /// Asks for a page of randomness in one call.
    LargeRandom,
    /// Branches eight times on what it drew, so it has two hundred and fifty-six paths.
    ///
    /// The only built-in guest worth searching; the others take one path whatever they draw.
    ManyFlips,
}

impl Demo {
    /// The fixture this guest is.
    const fn fixture(self) -> Fixture {
        match self {
            Self::ClockAndRandom => Fixture::ClockAndRandom,
            Self::TwoTasks => Fixture::TwoTasks,
            Self::LargeRandom => Fixture::LargeRandom,
            Self::ManyFlips => Fixture::ManyFlips,
        }
    }

    /// The program this guest runs.
    #[must_use]
    pub(crate) fn program(self) -> NullProgram {
        self.fixture().program()
    }

    /// Every guest, for a run that exercises all of them.
    #[must_use]
    pub(crate) fn all() -> [Self; 4] {
        [Self::ClockAndRandom, Self::TwoTasks, Self::LargeRandom, Self::ManyFlips]
    }

    /// Short name, as it appears on the command line and in output.
    #[must_use]
    pub(crate) const fn name(self) -> &'static str {
        self.fixture().name()
    }

    /// The guest of that name, where this build has one.
    #[must_use]
    pub(crate) fn named(name: &str) -> Option<Self> {
        Self::all().into_iter().find(|g| g.name() == name)
    }
}
