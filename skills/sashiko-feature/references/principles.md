# SOLID & DRY Principles in Rust

Adhere to these principles to ensure high-quality, maintainable, and idiomatic Rust code.

## SOLID Principles

### Single Responsibility Principle (SRP)
- **Concept:** Modules and structs should be small and focused.
- **In Rust:** Use the module system and traits to keep components cohesive. Prevent "God objects" that manage too much state or logic.

### Open/Closed Principle (OCP)
- **Concept:** Entities should be open for extension but closed for modification.
- **In Rust:** Extend existing types by implementing new traits (`impl Trait for Struct`). Avoid modifying core logic when adding new behaviors.

### Liskov Substitution Principle (LSP)
- **Concept:** Subtypes must be substitutable for their base types.
- **In Rust:** Ensure trait implementations maintain consistent, expected behavior. Consumers should be able to use any implementer of a trait interchangeably without surprising side effects.

### Interface Segregation Principle (ISP)
- **Concept:** Clients should not be forced to depend on methods they do not use.
- **In Rust:** Design small, cohesive traits. Break down large, complex traits into smaller ones that can be composed.

### Dependency Inversion Principle (DIP)
- **Concept:** High-level modules should depend on abstractions, not concrete types.
- **In Rust:** Depend on traits rather than concrete types in function signatures and struct fields. This is central to idiomatic Rust (using `impl Trait`, `dyn Trait`, or generic bounds).

## DRY (Don't Repeat Yourself) Principles

### Generics
- Abstract over types to avoid writing identical code for different data types.

### Traits
- Define shared behavior that can be reused across different structs.

### Macros
- Use macros to generate repetitive code patterns at compile-time (e.g., boilerplate for trait implementations).

### Modules
- Organize code into cohesive libraries to encourage reuse across different parts of the project.