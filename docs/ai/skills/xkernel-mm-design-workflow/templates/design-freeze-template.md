# Frozen Design

## Scope

## Linux Semantic Baseline

## Accepted X-Kernel Design Decisions

## Architecture

## Crate Decomposition

### For each crate

- Crate name
- Purpose
- Owned concepts
- Depends on
- Must not own
- Phase status: now / later

## Component Responsibilities

## Data Structures

### For each key structure

- Name
- Crate
- Role
- Core fields
- Invariants
- Lifecycle
- Locking notes

## Interfaces

### For each core interface

- Interface name
- Crate
- Signature sketch
- Purpose
- Inputs
- Outputs
- Errors
- Preconditions
- Postconditions
- Concurrency / context constraints

## Lifetime and Locking

## State Machines and Key Flows

## Crate-to-Flow Mapping

Show which crate participates in:

- mmap / munmap / mprotect
- page fault
- anonymous fault
- file-backed fault
- private COW
- fork interaction
- teardown / invalidation

## Compatibility Matrix

## Deferred Items

## Explicit Non-Goals

## Risks and Open Follow-ups
