//! Demonstrates validation error collection.

use sentinel_rs::validation::validators;
use sentinel_rs::ErrorCollector;

fn main() {
    let mut collector = ErrorCollector::new();

    // Simulate a registration form
    let username = "";
    let password = "short";
    let email = "not-an-email";

    validators::required(&Some(username.into()), &mut collector, "username");
    validators::min_length(password, 8, &mut collector, "password");
    validators::email(email, &mut collector, "email");

    match collector.finish() {
        Ok(()) => println!("validation passed"),
        Err(err) => {
            eprintln!("{err}");
            eprintln!();

            // Programmatic access
            for (field, messages) in err.field_errors().iter() {
                eprintln!("  {field}:");
                for m in messages {
                    eprintln!("    - {m}");
                }
            }
        }
    }
}
