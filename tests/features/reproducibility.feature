Feature: Reproducibility

  The claim the whole tool rests on: the same seed produces the same run, a different seed produces
  a different one, and a run that fails to reproduce says so rather than passing quietly.

  Scenario: The same seed produces the same run
    Given the built-in guest "clock-and-random"
    When I run it 100 times with seed 42
    Then the run succeeds
    And the report says every run agreed

  Scenario: A different seed produces a different run
    Given the built-in guest "clock-and-random"
    When I run it once with seed 1
    And I run it again with seed 2
    Then the two runs report different digests

  Scenario: Interleaving is reproduced as well as arithmetic
    Given the built-in guest "two-tasks"
    When I run it 100 times with seed 7
    Then the run succeeds
    And the report says every run agreed
