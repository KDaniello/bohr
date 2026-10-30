Feature: Refusals

  A program this build cannot carry deterministically is refused before it runs, under a code that
  says which wall it hit. Refusing loudly is the whole difference between a tool that is wrong and a
  tool that says so — and the code matters as much as the refusal, because it is what decides where
  the reader looks next.

  @linux
  Scenario: A syscall nothing models stops the run
    Given the program "testdata/guests/build/unmodelled_syscall"
    When I run the program once with seed 1
    Then the exit code is 3
    And the message names error code "BOHR-2001"

  @linux
  Scenario: Entering an asynchronous ring is refused under its own name
    Given the program "testdata/guests/build/enters_io_uring"
    When I run the program once with seed 1
    Then the exit code is 3
    And the message names error code "BOHR-2004"
