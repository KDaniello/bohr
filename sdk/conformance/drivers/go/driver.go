// A program that makes the calls a conformance case names, so what this SDK writes can be compared
// against what every other one writes for the same case.
//
// Deliberately a separate module that imports the SDK the way a customer would: that is what makes
// it able to check the one thing the SDK's own tests cannot, which is that an assertion is located
// in the program rather than in the SDK.
package main

import (
	"encoding/json"
	"fmt"
	"os"

	bohr "github.com/bohr-dev/bohr-sdk-go"
)

type call struct {
	Fn   string `json:"fn"`
	Args []any  `json:"args"`
}

type useCase struct {
	Calls []call `json:"calls"`
}

func main() {
	if len(os.Args) < 2 {
		fmt.Fprintln(os.Stderr, "usage: conformance-driver <case json>")
		os.Exit(2)
	}
	// The case arrives as its own text rather than as a path, the way every other driver takes it,
	// so the checker never has to know where a language's temporary files may live.
	var parsed useCase
	if err := json.Unmarshal([]byte(os.Args[1]), &parsed); err != nil {
		fmt.Fprintf(os.Stderr, "the case does not parse: %v\n", err)
		os.Exit(2)
	}

	for _, one := range parsed.Calls {
		if !perform(one) {
			// A call this driver has no case for means the suite is asking for something this
			// language does not offer, which is a difference worth failing over rather than
			// skipping quietly.
			fmt.Fprintf(os.Stderr, "this driver has no call named %s\n", one.Fn)
			os.Exit(2)
		}
	}
}

// perform makes one call, and says whether it knew how.
func perform(one call) bool {
	flag, _ := arg(one, 0).(bool)
	switch one.Fn {
	case "always":
		bohr.Always(flag, text(one, 1))
	case "always_or_unreachable":
		bohr.AlwaysOrUnreachable(flag, text(one, 1))
	case "sometimes":
		bohr.Sometimes(flag, text(one, 1))
	case "reachable":
		bohr.Reachable(text(one, 0))
	case "unreachable":
		bohr.Unreachable(text(one, 0))
	case "fault_point":
		_ = bohr.FaultPoint(text(one, 0))
	case "setup_complete":
		bohr.SetupComplete(arg(one, 0))
	case "send_event":
		bohr.SendEvent(text(one, 0), arg(one, 1))
	case "random_bytes":
		count, _ := arg(one, 0).(float64)
		_ = bohr.RandomBytes(int(count))
	default:
		return false
	}
	return true
}

func arg(one call, at int) any {
	if at < len(one.Args) {
		return one.Args[at]
	}
	return nil
}

func text(one call, at int) string {
	value, _ := arg(one, at).(string)
	return value
}
