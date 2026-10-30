Feature: Bundles

  A bundle is what leaves the machine that found something. It carries what a replay needs, and it
  refuses to be read as anything other than what it is.

  Scenario: A saved run replays to the digest it recorded
    Given the built-in guest "clock-and-random"
    When I run it once with seed 3 and keep the bundle
    And I replay the bundle
    Then the replay succeeds
    And the replay reports the recorded digest

  Scenario: A bundle that has been altered is refused
    Given the built-in guest "clock-and-random"
    When I run it once with seed 3 and keep the bundle
    And I alter a byte in the middle of the bundle
    And I replay the bundle
    Then the replay fails
    And the message names an error code

  Scenario: A file that is not a bundle is refused rather than guessed at
    Given a file that is not a bundle
    When I replay the bundle
    Then the replay fails
    And the message names an error code
