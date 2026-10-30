module github.com/bohr-dev/bohr-sdk-go

// No require block, and there never will be one: this package is compiled into a customer's
// program, and a library in that position is a version conflict waiting for them. Checked by
// scripts/check-sdk-deps.sh. See docs/ARCHITECTURE.md §9 rule S3.
go 1.21
