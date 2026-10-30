package bohr

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// A record as the channel writes it, read back.
type record map[string]any

func linesFrom(t *testing.T, path string) []record {
	t.Helper()
	text, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("reading what was written: %v", err)
	}
	var out []record
	for _, line := range strings.Split(strings.TrimSpace(string(text)), "\n") {
		if line == "" {
			continue
		}
		var parsed record
		if err := json.Unmarshal([]byte(line), &parsed); err != nil {
			t.Fatalf("a line that is not a record: %q: %v", line, err)
		}
		out = append(out, parsed)
	}
	return out
}

// Everything about a live channel is checked in one test, because there is only ever one: the
// channel is resolved once for the life of a process and stays open, which is what a program
// wants and what leaves a second test nothing to resolve.
//
// The directory is not the one the framework cleans up, for the same reason: on some platforms an
// open file cannot be removed, and a test that failed at cleanup would be reporting the design
// rather than a defect.
func TestARunWritesWhatItPromised(t *testing.T) {
	dir, err := os.MkdirTemp("", "bohr-sdk-go")
	if err != nil {
		t.Fatalf("somewhere to write: %v", err)
	}
	path := filepath.Join(dir, "records.jsonl")
	t.Setenv(outputVar, path)
	t.Setenv(nodeVar, "3")
	t.Setenv(armedVar, "before_commit_ack,after_leader_change")

	Always(true, "the log is monotonic")
	Sometimes(false, "a leader changed under a partition")
	Reachable("the retry path")
	armed := FaultPoint("before_commit_ack")
	quiet := FaultPoint("never_armed")
	SetupComplete(map[string]any{"nodes": 5})
	SendEvent("elected", map[string]any{"term": 7})

	if !armed {
		t.Error("a fault point this run armed reported itself as quiet")
	}
	if quiet {
		t.Error("a fault point nothing armed reported itself as armed")
	}

	written := linesFrom(t, path)
	if len(written) != 7 {
		t.Fatalf("wrote %d records, wanted 7", len(written))
	}

	// Every record says which version wrote it and where it sits in the sequence. A gap is how
	// the supervisor notices a record it never saw.
	for at, line := range written {
		meta, ok := line["bohr"].(map[string]any)
		if !ok {
			t.Fatalf("record %d carries no metadata: %v", at, line)
		}
		if meta["schema"] != float64(Schema) {
			t.Errorf("record %d claims schema %v", at, meta["schema"])
		}
		if meta["seq"] != float64(at+1) {
			t.Errorf("record %d is numbered %v", at, meta["seq"])
		}
		if meta["node"] != float64(3) {
			t.Errorf("record %d says it came from node %v", at, meta["node"])
		}
	}

	first, ok := written[0]["antithesis_assert"].(map[string]any)
	if !ok {
		t.Fatalf("the first record is not an assertion: %v", written[0])
	}
	if first["assert_type"] != "always" || first["condition"] != true {
		t.Errorf("the promise was not written as made: %v", first)
	}
	if first["message"] != "the log is monotonic" {
		t.Errorf("the promise lost its words: %v", first["message"])
	}

	// That every assertion carries a location is checkable here. *Which* line it names is not:
	// the walk leaves this package by directory, and a test in this package is in that directory,
	// so from here it can only ever point past itself into the test framework. What checks the
	// walk is the conformance driver, which is a program in another package doing what a
	// customer's would.
	at, ok := first["location"].(map[string]any)
	if !ok {
		t.Fatalf("an assertion with no location: %v", first)
	}
	if file, _ := at["file"].(string); file == "" {
		t.Error("an assertion located nowhere")
	}
}

// The ordinary case: a program with this compiled in, running outside Bohr, does nothing and
// costs nothing. Anything else and nobody would leave it in a release build.
func TestNothingIsResolvedWhenNobodyIsListening(t *testing.T) {
	saved := current
	current = nil
	t.Setenv(outputVar, "")
	t.Setenv(antithesisVar, "")

	resolve()
	if current != nil {
		t.Error("a channel was opened with nothing naming one")
	}
	current = saved
}

// A name that is not a path names a descriptor the supervisor already opened, which is how it
// hands one over when there is no path the guest could see. A number that names nothing must
// leave no channel rather than a channel that swallows everything.
func TestANumberThatNamesNothingIsNotAChannel(t *testing.T) {
	saved := current
	current = nil
	t.Setenv(outputVar, "999999")

	resolve()
	if current != nil {
		t.Error("a descriptor that is not open was accepted as a channel")
	}
	current = saved
}

// The other SDKs spell `<` and `&` as themselves. A record that escaped them would be the one
// document in the format that reads differently, and the conformance suite parses both sides so
// it would never notice.
func TestMarkupCharactersAreNotEscaped(t *testing.T) {
	line, err := encode("antithesis_event", map[string]any{"a<b&c": ">"}, 1, 0)
	if err != nil {
		t.Fatalf("encoding: %v", err)
	}
	if !strings.Contains(string(line), "a<b&c") {
		t.Errorf("the characters were escaped: %s", line)
	}
}
