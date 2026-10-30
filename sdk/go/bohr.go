// Package bohr lets a program say what it promises about itself, so a deterministic simulator can
// tell it apart from a program that merely did not crash.
//
// Every function here costs one atomic load and a nil check when nothing is listening, which is
// what a program compiled with this in it spends most of its life doing. Nothing is raised, ever:
// this package is a bystander in somebody else's program and is in no position to end one.
//
// The surface is the same in every language Bohr ships — see docs/API.md §6.1 — in the spelling
// each one would have used. Go has no macros, so these are plain functions and the location an
// assertion reports is found from the call stack.
package bohr

import (
	"encoding/binary"
	"io"
	"os"
)

// Always claims that condition holds every time this line is reached.
func Always(condition bool, message string) {
	assert("always", "Always", condition, message)
}

// AlwaysOrUnreachable claims that condition holds whenever this line is reached, and that never
// reaching it is acceptable.
func AlwaysOrUnreachable(condition bool, message string) {
	assert("always_or_unreachable", "AlwaysOrUnreachable", condition, message)
}

// Sometimes claims that condition holds at least once across a campaign.
//
// A campaign-level claim: one run cannot answer it, and a campaign that never satisfies it reports
// it as a gap in the testing rather than as a defect in the program.
func Sometimes(condition bool, message string) {
	assert("sometimes", "Sometimes", condition, message)
}

// Reachable records that this line was reached at least once.
func Reachable(message string) {
	assert("reachable", "Reachable", true, message)
}

// Unreachable records that a line which should never be reached was.
func Unreachable(message string) {
	assert("unreachable", "Unreachable", false, message)
}

// FaultPoint reports that the program passed a place a fault could be injected, and says whether
// this run armed it.
//
// Outside Bohr it returns false and the program takes its ordinary path.
func FaultPoint(name string) bool {
	c := open()
	_, armed := false, false
	if c != nil {
		_, armed = c.armed[name]
	}
	emit("bohr_fault_point", map[string]any{"name": name, "armed": armed})
	return armed
}

// SetupComplete announces that the program has finished setting itself up.
//
// What it separates is a program still arranging itself from a program running, which is what a
// search needs to know before it starts breaking things.
func SetupComplete(details any) {
	emit("antithesis_setup", map[string]any{"status": "complete", "details": details})
}

// SendEvent puts a named event on the run's timeline.
func SendEvent(name string, details any) {
	emit("antithesis_event", map[string]any{name: details})
}

// RandomBytes returns bytes that belong to the run rather than to the machine.
//
// Under Bohr these come from the entropy tape by way of the world model, so one seed reproduces
// them. The read is from /dev/urandom deliberately and not from crypto/rand: on Linux that would
// issue getrandom, and the whole mechanism depends on the supervisor answering this read from the
// tape. Reading a different source would make Go the one language whose randomness does not
// reproduce.
func RandomBytes(n int) []byte {
	if n <= 0 {
		return nil
	}
	if open() != nil {
		SendEvent("bohr_random", map[string]any{"n": n})
	}
	source, err := os.Open("/dev/urandom")
	if err != nil {
		return make([]byte, n)
	}
	defer func() { _ = source.Close() }()

	out := make([]byte, n)
	if _, err := io.ReadFull(source, out); err != nil {
		return make([]byte, n)
	}
	return out
}

// RandomChoice returns one of choices, and false where there is nothing to choose from.
func RandomChoice[T any](choices []T) (T, bool) {
	var nothing T
	if len(choices) == 0 {
		return nothing, false
	}
	drawn := binary.LittleEndian.Uint64(RandomBytes(8))
	return choices[drawn%uint64(len(choices))], true
}

// assert writes one promise, and does nothing at all when nobody is listening.
//
// The check comes before anything is built: `here` walks a stack, and a program outside Bohr must
// not pay for that on every assertion it makes.
func assert(kind, display string, condition bool, message string) {
	if open() == nil {
		return
	}
	emit("antithesis_assert", map[string]any{
		"hit":          true,
		"must_hit":     kind != "unreachable",
		"assert_type":  kind,
		"display_type": display,
		"message":      message,
		"condition":    condition,
		"id":           message,
		"location":     here(),
	})
}
