Feature: Diagnosis

  Before a host is blamed for a run it can be asked what it is able to do. The shape of the answer
  is the same whether the host is capable or not: a verdict, in writing, with something to type
  under anything that failed.

  Scenario: A host reaches a verdict about itself
    When I ask this host what it can do
    Then a verdict is reached
    And every failure carries something to type

  Scenario: The syscall support table can be read
    When I ask for the syscall table
    Then the answer is not empty
