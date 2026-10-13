# C++ Standard Library (`libstdc++-v3/`) Invariants

## 1. `constexpr`, Exception Safety, and Value Category Forwarding
- Check that `constexpr` functions in `libstdc++-v3/include/` do not invoke non-`constexpr` helpers or trigger undefined behavior during constant evaluation.
- In `<ranges>`, `<format>`, and container templates, verify that user-defined iterators, sentinels, predicates, and projections are forwarded with the right value category and `const` qualification, and that single-pass input iterators are not dereferenced or incremented twice when caching/advancing.
- Check `noexcept(...)` specifications: if a wrapper delegates to a user-customizable operation, a hardcoded `noexcept(true)` can call `std::terminate` when the underlying operation throws, while a wrong `noexcept(...)` expression with side-effecting or ill-formed sub-expressions can fail to compile.

## 2. Lifetime, Dangling Views, and Self-Assignment / Reallocation
- In containers (`std::vector`, `std::basic_string`, `std::flat_map`, etc.), inserting elements that alias the container itself (e.g., `v.push_back(v[0])` or `v.insert(pos, v.begin(), v.end())`) must read or copy the source before reallocating or shifting elements in place.
- In `<format>` and `<ranges>`, verify temporary `basic_format_args` or view adaptors do not store dangling references/spans past the full-expression lifetime.

## 3. `[[no_unique_address]]` Wrappers and User-Supplied Types
- When wrapping user-supplied types (allocators, hashers, predicates, comparators, sentinels, or functors) in helper structs using `[[__no_unique_address__]]` instead of empty-base inheritance, initializing the wrapper with `{}` performs **aggregate initialization** if the wrapper is an aggregate without a default member initializer (NSDMI), which copy-list-initializes the underlying member from `{}`.
- Copy-list-initialization from `{}` is **ill-formed when the user-supplied type has an `explicit` default constructor**! Always provide an NSDMI (`_Tp _M_v{};`) or a non-aggregate value-initializing constructor on wrapper structs.
