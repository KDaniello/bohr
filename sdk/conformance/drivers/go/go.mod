module github.com/bohr-dev/bohr/sdk/conformance/drivers/go

go 1.21

require github.com/bohr-dev/bohr-sdk-go v0.0.0

// The SDK is beside this in the repository rather than fetched, so the suite builds with no
// network and the driver is always testing the code in this checkout.
replace github.com/bohr-dev/bohr-sdk-go => ../../../go
