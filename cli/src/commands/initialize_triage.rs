//! `ralphus initialize triage`: deterministic pool registration exercise.
//!
//! Two held candidates enroll in a Triage pool whose threshold is 2, which
//! drains the pool into a review. With `--remote`, the candidates are declared
//! on the strict loopback machine, so pooling runs through remote-cell review
//! derivation instead of reading a local worktree.

use crate::commands::initialize_exercise::{Exercise, ExerciseOptions, Fixture, squad_id};

pub fn dispatch(options: &ExerciseOptions) -> i32 {
    let exercise = match Exercise::start("triage", options) {
        Ok(value) => value,
        Err(error) => return fail(&error),
    };
    let fixture = match exercise.fixture_project("triage") {
        Ok(value) => value,
        Err(error) => return fail(&error),
    };
    let project = fixture.name.clone();
    if let Err(error) = exercise.client.register_triage_type(
        "exercise",
        "Exercise",
        "A deterministic guided-exercise type.",
    ) {
        return fail(&format!("could not register type: {error}"));
    }
    if let Err(error) = exercise
        .client
        .set_triage_pool_threshold(&project, "exercise", Some(2))
    {
        return fail(&format!("could not set pool threshold: {error}"));
    }
    let examples = match exercise.examples_dir() {
        Ok(value) => value,
        Err(error) => return fail(&error),
    };
    for n in 1..=2 {
        let toml = fixture_toml(&exercise, &fixture, n);
        if let Err(error) =
            std::fs::write(examples.join(format!("triage-exercise-{n}.toml")), &toml)
        {
            return fail(&error.to_string());
        }
        let _squad = match exercise
            .client
            // Triage enrollment happens at submission time. Holding these
            // candidates demonstrates pooling and threshold draining without
            // consuming an agent or leaving work running after the exercise.
            .submit(&toml, true, Some(&format!("triage exercise {n}")))
            .map_err(|e| e.to_string())
            .and_then(|v| squad_id(&v))
        {
            Ok(value) => value,
            Err(error) => return fail(&error),
        };
    }
    let drained = exercise
        .client
        .list_triage_pools()
        .map_err(|e| e.to_string())
        .ok()
        .and_then(|v| v["pools"].as_array().cloned())
        .is_some_and(|pools| {
            pools.iter().any(|pool| {
                pool["project"] == project
                    && pool["triage_type"] == "exercise"
                    && pool["count"] == 0
            })
        });
    if !drained {
        return fail("the two completed candidates did not drain the exercise triage pool");
    }
    println!("isolated triage exercise is ready.");
    println!("  daemon: {}", exercise.url);
    println!("  project/type: {project}/exercise; threshold: 2");
    println!("  fixtures: {}", examples.display());
    println!(
        "submitted two raw candidates and verified the threshold drained the pool into a review."
    );
    0
}

/// One held Triage candidate. A remote candidate needs its own worktree
/// branch: a machine's branch must be knowable from the placeholder alone.
fn fixture_toml(exercise: &Exercise, fixture: &Fixture, n: u32) -> String {
    let project = &fixture.name;
    let machine = exercise.machine_line();
    let cwd = exercise.cell_cwd(fixture, &format!("triage-candidate-{n}"));
    format!(
        "[[task]]\nname = \"triage-exercise\"\nproject = {project:?}\n{machine}\n[[task.cell]]\ncwd = {cwd:?}\ncommand = \"exit 0\"\nmode = \"raw\"\ntriage = true\ntriage_type = \"exercise\"\n"
    )
}

fn fail(message: &str) -> i32 {
    println!("error: {message}");
    1
}
