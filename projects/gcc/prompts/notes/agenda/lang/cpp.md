# C++ Standard Library and Memory Management

This module covers C++ library contracts, including type traits, iterator concepts, allocator-aware construction, and manual lifetime management for non-RAII containers.

Derived by `sashiko notes agenda` from 200 fixes reachable from 4990533b6a10301ff4681fbcc0bbe7c32ca819fe (115 code bugs, 23 themes).

## CPPLIB: Standard Library Contracts, Allocators, and Type Traits

- CPPLIB-1: How are type trait categories and iterator concept constraints enforced before instantiating library templates?
- CPPLIB-2: When constructing temporary objects in allocator-aware containers, how is the allocator propagated to internal elements?
- CPPLIB-3: Which pure query algorithms and accessors must carry discard-warning attributes to uphold library interface contracts?
- CPPLIB-4: How are calendar and time-zone offset representations bounded and converted to prevent truncation?

## CPPMEM: Manual Memory Management of Non-RAII Containers

- CPPMEM-1: When using container types designed without destructors for union or garbage-collector compatibility, how is local memory released?
- CPPMEM-2: Under what conditions must a non-RAII container be wrapped in an automatic lifetime manager when declared as a local variable?
- CPPMEM-3: How are early return and error-handling paths structured to guarantee deallocation of manually managed buffers?
