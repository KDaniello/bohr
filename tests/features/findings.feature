Feature: Findings

  A finding is what the tool exists to produce, so it is reported as a success of the tool and a
  failure of the guest: named on the timeline, and carried by the exit code.

  Scenario: A guest that does nothing wrong is clean
    Given the built-in guest "clock-and-random"
    When I run it once with seed 0
    Then the run succeeds
    And the exit code is 0

  @linux
  Scenario: A guest that crashes is a finding
    Given the program "testdata/guests/build/segfault"
    When I run the program once with seed 0
    Then the exit code is 1
    And the report names the finding
