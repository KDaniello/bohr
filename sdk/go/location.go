package bohr

import (
	"path"
	"runtime"
	"strings"
)

// Where in the guest's own source something is written.
type location struct {
	File      string `json:"file"`
	Function  string `json:"function"`
	BeginLine int    `json:"begin_line"`
	Column    int    `json:"begin_column"`
}

// The directory this package's own frames report, learned once from a frame that is certainly ours.
//
// Compared by directory rather than by file so that a helper added in another file of this package
// does not start appearing in customers' reports as the place their assertion is written.
var sdkDir = func() string {
	_, file, _, ok := runtime.Caller(0)
	if !ok {
		return ""
	}
	return path.Dir(file)
}()

// here is the first frame outside this package.
//
// Walked rather than counted: a fixed number of frames to skip is correct until somebody adds a
// helper, and then every report quietly points at the SDK instead of at the program it is
// reporting on.
//
// Through CallersFrames rather than Caller because inlining removes physical frames and
// CallersFrames expands the inlined ones back — without it a customer's optimised build would
// report a different line from their debug build, for the same source.
func here() location {
	var pcs [32]uintptr
	taken := runtime.Callers(2, pcs[:])
	frames := runtime.CallersFrames(pcs[:taken])
	for {
		frame, more := frames.Next()
		if frame.File != "" && path.Dir(frame.File) != sdkDir {
			return location{
				File:      frame.File,
				Function:  shortName(frame.Function),
				BeginLine: frame.Line,
			}
		}
		if !more {
			return location{}
		}
	}
}

// shortName drops the package path from a function name, leaving what a person calls it.
func shortName(qualified string) string {
	at := strings.LastIndex(qualified, "/")
	if at >= 0 {
		qualified = qualified[at+1:]
	}
	if at := strings.Index(qualified, "."); at >= 0 {
		return qualified[at+1:]
	}
	return qualified
}
