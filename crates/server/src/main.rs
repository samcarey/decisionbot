use crate::command::Command;
use anyhow::{bail, Context, Result};
use axum::{
    response::{Html, IntoResponse},
    routing::post,
    Extension, Form, Router,
};
use dotenv::dotenv;
use enum_iterator::all;
use ical::parser::vcard::component::VcardContact;
use log::*;
use once_cell::sync::Lazy;
use openapi::apis::{
    api20100401_message_api::{create_message, CreateMessageParams},
    configuration::Configuration,
};
use sqlx::{query, query_as, Pool, Sqlite};
use std::env;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use std::{collections::HashMap, str::FromStr};
use util::E164;

mod command;
#[cfg(test)]
mod test;
mod util;

#[tokio::main]
async fn main() -> Result<()> {
    dotenv()?;
    env_logger::init();
    info!("Starting up");
    let twilio_config = Configuration {
        basic_auth: Some((
            env::var("TWILIO_API_KEY_SID")?,
            Some(env::var("TWILIO_API_KEY_SECRET")?),
        )),
        ..Default::default()
    };
    send(
        &twilio_config,
        env::var("CLIENT_NUMBER")?,
        "Server is starting up".to_string(),
    )
    .await?;
    let pool = sqlx::SqlitePool::connect(&env::var("DATABASE_URL")?).await?;
    query!("PRAGMA foreign_keys = ON").execute(&pool).await?; // SQLite has this off by default
    let app = Router::new()
        .route("/", post(handle_incoming_sms))
        .layer(Extension(pool));
    let listener = tokio::net::TcpListener::bind(format!(
        "{}:{}",
        env::var("CALLBACK_IP")?,
        env::var("CALLBACK_PORT")?
    ))
    .await?;
    info!("Listening on {}", listener.local_addr()?);
    axum::serve(listener, app).await?;

    Ok(())
}

// field names must be exact (including case) to match API
#[allow(non_snake_case)]
#[derive(serde::Deserialize, Default, Debug)]
struct SmsMessage {
    Body: String,
    From: String,
    NumMedia: Option<String>,
    MediaContentType0: Option<String>,
    MediaUrl0: Option<String>,
}

struct User {
    number: String,
    #[allow(dead_code)]
    name: String,
}

#[derive(Clone)]
struct Contact {
    id: i64,
    contact_name: String,
    contact_user_number: String,
}

// Handler for incoming SMS messages
async fn handle_incoming_sms(
    Extension(pool): Extension<Pool<Sqlite>>,
    Form(message): Form<SmsMessage>,
) -> impl IntoResponse {
    let response = match process_message(&pool, message).await {
        Ok(response) => response,
        Err(error) => {
            error!("Error: {error:?}");
            "Internal Server Error!".to_string()
        }
    };
    debug!("Sending response: {response}");
    Html(format!(
        r#"
        <?xml version="1.0" encoding="UTF-8"?>
        <Response>
        <Message>{response}</Message>
        </Response>
        "#
    ))
}

async fn process_message(pool: &Pool<Sqlite>, message: SmsMessage) -> anyhow::Result<String> {
    trace!("Received {message:?}");
    let SmsMessage {
        Body: body,
        From: from,
        NumMedia,
        MediaContentType0,
        MediaUrl0,
    } = message;
    debug!("Received from {from}: {body}");
    if NumMedia == Some("1".to_string())
        && MediaContentType0
            .map(|t| ["text/vcard", "text/x-vcard"].contains(&t.as_str()))
            .unwrap_or(false)
    {
        let vcard_data = reqwest::get(&MediaUrl0.unwrap()).await?.text().await?;
        let reader = ical::VcardParser::new(vcard_data.as_bytes());
        let mut stats = ImportStats::default();

        for vcard in reader {
            match process_vcard(pool, &from, vcard).await {
                Ok(ImportResult::Added) => stats.added += 1,
                Ok(ImportResult::Updated) => stats.updated += 1,
                Ok(ImportResult::Unchanged) => stats.skipped += 1,
                Ok(ImportResult::Deferred) => stats.deferred += 1,
                Err(e) => stats.add_error(&e.to_string()),
            }
        }

        return Ok(stats.format_report(&from));
    }

    let mut words = body.trim().split_ascii_whitespace();
    let command_word = words.next();
    let command = command_word.map(Command::try_from);

    let Some(User {
        number, name: _, ..
    }) = query_as!(User, "select * from users where number = ?", from)
        .fetch_optional(pool)
        .await?
    else {
        return onboard_new_user(command, words, &from, pool).await;
    };

    let Some(command) = command else {
        return Ok(Command::h.hint());
    };

    let Ok(command) = command else {
        return Ok(format!(
            "We didn't recognize that command word: \"{}\".\n{}",
            command_word.unwrap(),
            Command::h.hint()
        ));
    };

    let response = match command {
        // I would use HELP for the help command, but Twilio intercepts and does not relay that
        Command::h => {
            let available_commands = format!(
                "Available commands:\n{}\n",
                all::<Command>()
                    .map(|c| format!("- {c}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            format!("{available_commands}\n{}", Command::info.hint())
        }
        Command::name => match process_name(words) {
            Ok(name) => {
                query!("update users set name = ? where number = ?", name, from)
                    .execute(pool)
                    .await?;
                format!("Your name has been updated to \"{name}\"")
            }
            Err(hint) => hint.to_string(),
        },
        Command::stop => {
            query!("delete from users where number = ?", number)
                .execute(pool)
                .await?;
            // They won't actually see this when using Twilio
            "You've been unsubscribed. Goodbye!".to_string()
        }
        Command::info => {
            let command_text = words.next();
            if let Some(command) = command_text.map(Command::try_from) {
                if let Ok(command) = command {
                    format!(
                        "{}, to {}.{}",
                        command.usage(),
                        command.description(),
                        command.example()
                    )
                } else {
                    format!("Command \"{}\" not recognized", command_text.unwrap())
                }
            } else {
                Command::info.hint()
            }
        }
        Command::contacts => {
            let contacts = query_as!(
                Contact,
                "SELECT id as \"id!\", contact_name, contact_user_number FROM contacts WHERE submitter_number = ? ORDER BY contact_name",
                from
            )
            .fetch_all(pool)
            .await?;

            if contacts.is_empty() {
                "You don't have any contacts.".to_string()
            } else {
                let contact_list = contacts
                    .iter()
                    .enumerate()
                    .map(|(i, c)| {
                        format!(
                            "{}. {} ({})",
                            i + 1,
                            c.contact_name,
                            &E164::from_str(&c.contact_user_number)
                                .expect("Should have been formatted upon db insertion")
                                .area_code()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                format!("Your contacts:\n{}", contact_list)
            }
        }
        Command::delete => {
            let name = words.collect::<Vec<_>>().join(" ");
            if name.is_empty() {
                Command::delete.hint()
            } else {
                handle_delete(pool, &from, &name).await?
            }
        }
        Command::confirm => {
            let nums = words.collect::<Vec<_>>().join(" ");
            if nums.is_empty() {
                Command::confirm.hint()
            } else {
                handle_confirm(pool, &from, &nums).await?
            }
        }
        Command::pick => {
            let nums = words.collect::<Vec<_>>().join(" ");
            if nums.is_empty() {
                Command::pick.hint()
            } else {
                handle_pick(pool, &from, &nums).await?
            }
        }
    };
    Ok(response)
}

async fn handle_pick(pool: &Pool<Sqlite>, from: &str, selections: &str) -> anyhow::Result<String> {
    // Get the deferred contacts while holding the lock
    let deferred_contacts = {
        let mut deferred_map = DEFERRED_CONTACTS.lock().unwrap();

        // Clean up expired contacts while we have the lock
        deferred_map.retain(|_, contacts| {
            contacts.retain(|c| c.timestamp.elapsed() <= DEFERRED_TIMEOUT);
            !contacts.is_empty()
        });

        // Clone the contacts we need so we can release the lock
        deferred_map.get(from).map(|contacts| contacts.clone())
    };

    let Some(deferred_contacts) = deferred_contacts else {
        return Ok("No pending contacts to pick from.".to_string());
    };

    let mut successful = Vec::new();
    let mut failed = Vec::new();

    // Parse selections like "1a, 2b, 3a"
    for selection in selections.split(',').map(str::trim) {
        if selection.len() < 2 {
            failed.push(format!("Invalid selection format: {}", selection));
            continue;
        }

        // Split into numeric and letter parts
        let (num_str, letter) = selection.split_at(selection.len() - 1);
        let contact_idx: usize = match num_str.parse::<usize>() {
            Ok(n) if n > 0 => n - 1,
            _ => {
                failed.push(format!("Invalid contact number: {}", num_str));
                continue;
            }
        };

        let letter_idx = match letter.chars().next().unwrap() {
            c @ 'a'..='z' => (c as u8 - b'a') as usize,
            _ => {
                failed.push(format!("Invalid letter selection: {}", letter));
                continue;
            }
        };

        // Get the contact and number
        let contact = match deferred_contacts.get(contact_idx) {
            Some(c) => c,
            None => {
                failed.push(format!("Contact number {} not found", contact_idx + 1));
                continue;
            }
        };

        let (number, _) = match contact.numbers.get(letter_idx) {
            Some(n) => n,
            None => {
                failed.push(format!(
                    "Number {} not found for contact {}",
                    letter,
                    contact_idx + 1
                ));
                continue;
            }
        };

        // Insert the contact
        if let Err(e) = add_contact(pool, from, &contact.name, number).await {
            failed.push(format!(
                "Failed to add {} ({}): {}",
                contact.name, number, e
            ));
        } else {
            successful.push(format!("{} ({})", contact.name, number));
        }
    }

    // Remove processed contacts after we're done
    {
        if let Ok(mut deferred_map) = DEFERRED_CONTACTS.lock() {
            if let Some(contacts) = deferred_map.get_mut(from) {
                contacts.retain(|_| false);
            }
        }
    }

    // Format response
    let mut response = String::new();
    if !successful.is_empty() {
        response.push_str(&format!(
            "Successfully added {} contact{}:\n",
            successful.len(),
            if successful.len() == 1 { "" } else { "s" }
        ));
        for contact in successful {
            response.push_str(&format!("• {}\n", contact));
        }
    }

    if !failed.is_empty() {
        if !response.is_empty() {
            response.push_str("\n");
        }
        response.push_str("Failed to process:\n");
        for error in failed {
            response.push_str(&format!("• {}\n", error));
        }
    }

    Ok(response)
}

async fn handle_delete(pool: &Pool<Sqlite>, from: &str, name: &str) -> anyhow::Result<String> {
    cleanup_pending_deletions();

    let like = format!("%{}%", name.to_lowercase());
    let contacts = query_as!(
        Contact,
        "SELECT id as \"id!\", contact_name, contact_user_number 
         FROM contacts 
         WHERE submitter_number = ? 
         AND LOWER(contact_name) LIKE ?
         ORDER BY contact_name",
        from,
        like
    )
    .fetch_all(pool)
    .await?;

    let list = contacts
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let area_code = E164::from_str(&c.contact_user_number)
                .map(|e| e.area_code().to_string())
                .unwrap_or_else(|_| "???".to_string());

            format!("{}. {} ({})", i + 1, c.contact_name, area_code)
        })
        .collect::<Vec<_>>()
        .join("\n");

    let response = format!(
        "Found these contacts matching \"{}\":\n{}\n\n\
        To delete contacts, reply \"confirm NUM1, NUM2, ...\", \
        where NUM1, NUM2, etc. are numbers from the list above.",
        name, list
    );

    // For each contact, generate a unique token and store the deletion request
    for (i, contact) in contacts.iter().enumerate() {
        let token = format!("{}:{}", from, i + 1);
        PENDING_DELETIONS.lock().unwrap().insert(
            token,
            PendingDeletion {
                contact_id: contact.id,
                timestamp: Instant::now(),
            },
        );
    }

    Ok(response)
}

async fn handle_confirm(
    pool: &Pool<Sqlite>,
    from: &str,
    selections: &str,
) -> anyhow::Result<String> {
    cleanup_pending_deletions();

    // Collect deletion IDs and release the lock immediately
    let to_delete = {
        let pending = PENDING_DELETIONS.lock().unwrap();
        let mut ids = Vec::new();
        let mut invalid = Vec::new();

        for num_str in selections.split(',').map(str::trim) {
            match num_str.parse::<usize>() {
                Ok(num) if num > 0 => {
                    let token = format!("{}:{}", from, num);
                    if let Some(deletion) = pending.get(&token) {
                        ids.push(deletion.contact_id);
                    } else {
                        invalid.push(format!("Invalid selection: {}", num));
                    }
                }
                _ => invalid.push(format!("Invalid number: {}", num_str)),
            }
        }

        if ids.is_empty() {
            if invalid.is_empty() {
                return Ok("No valid selections provided.".to_string());
            } else {
                return Ok(format!("Errors:\n{}", invalid.join("\n")));
            }
        }

        (ids, invalid)
    };

    let (to_delete, invalid) = to_delete;

    // Fetch contact details before deletion
    let mut contacts = Vec::new();
    for id in &to_delete {
        if let Some(contact) = query_as!(
            Contact,
            "SELECT id as \"id!\", contact_name, contact_user_number FROM contacts WHERE id = ?",
            id
        )
        .fetch_optional(pool)
        .await?
        {
            contacts.push(contact);
        }
    }

    // Perform deletions
    let mut tx = pool.begin().await?;
    for id in to_delete {
        query!("DELETE FROM contacts WHERE id = ?", id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;

    // Clear the processed deletions from pending map
    {
        let mut pending = PENDING_DELETIONS.lock().unwrap();
        for contact in &contacts {
            pending.retain(|_, deletion| deletion.contact_id != contact.id);
        }
    }

    // Format response
    let mut response = format!(
        "Deleted {} contact{}:\n",
        contacts.len(),
        if contacts.len() == 1 { "" } else { "s" }
    );

    for contact in contacts {
        let area_code = E164::from_str(&contact.contact_user_number)
            .map(|e| e.area_code().to_string())
            .unwrap_or_else(|_| "???".to_string());
        response.push_str(&format!("• {} ({})\n", contact.contact_name, area_code));
    }

    if !invalid.is_empty() {
        response.push_str("\nErrors:\n");
        response.push_str(&invalid.join("\n"));
    }

    Ok(response)
}

async fn add_contact(
    pool: &Pool<Sqlite>,
    from: &str,
    name: &str,
    number: &str,
) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;

    // Create user if needed
    let contact_user = query!("SELECT * FROM users WHERE number = ?", number)
        .fetch_optional(&mut *tx)
        .await?;

    if contact_user.is_none() {
        query!(
            "INSERT INTO users (number, name) VALUES (?, ?)",
            number,
            name
        )
        .execute(&mut *tx)
        .await?;
    }

    // Insert contact
    query!(
        "INSERT INTO contacts (submitter_number, contact_name, contact_user_number) 
         VALUES (?, ?, ?)",
        from,
        name,
        number
    )
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}

fn cleanup_pending_deletions() {
    PENDING_DELETIONS
        .lock()
        .unwrap()
        .retain(|_, deletion| deletion.timestamp.elapsed() <= DELETION_TIMEOUT);
}

#[derive(Debug)]
enum ImportResult {
    Added,
    Updated,
    Unchanged,
    Deferred,
}

async fn process_vcard(
    pool: &Pool<Sqlite>,
    from: &str,
    vcard: Result<VcardContact, ical::parser::ParserError>,
) -> anyhow::Result<ImportResult> {
    let user_exists = query!("SELECT * FROM users WHERE number = ?", from)
        .fetch_optional(pool)
        .await?
        .is_some();
    if !user_exists {
        bail!("Please set your name first using the 'name' command before adding contacts");
    }

    let card = vcard?;

    let name = card
        .properties
        .iter()
        .find(|p| p.name == "FN")
        .and_then(|p| p.value.as_ref())
        .ok_or_else(|| anyhow::anyhow!("No name provided"))?;

    // Collect all TEL properties with their types/descriptions
    let mut numbers = Vec::new();
    for prop in card.properties.iter().filter(|p| p.name == "TEL") {
        if let Some(raw_number) = &prop.value {
            if let Ok(normalized) = E164::from_str(raw_number) {
                let description = prop.params.as_ref().and_then(|params| {
                    params
                        .iter()
                        .find(|(key, _)| key.eq_ignore_ascii_case("TYPE"))
                        .and_then(|(_, values)| values.first())
                        .map(|v| v.to_string())
                });
                numbers.push((normalized.to_string(), description));
            }
        }
    }

    if numbers.is_empty() {
        bail!("No valid phone numbers provided");
    }

    // Check existing contacts
    let existing_contacts = query!(
        "SELECT contact_user_number, contact_name FROM contacts WHERE submitter_number = ?",
        from
    )
    .fetch_all(pool)
    .await?;

    let mut new_numbers = Vec::new();
    let mut updated = false;

    for (num, desc) in numbers {
        if let Some(existing) = existing_contacts
            .iter()
            .find(|contact| contact.contact_user_number == num)
        {
            if existing.contact_name != *name {
                // Update the contact's name if it changed
                query!(
                    "UPDATE contacts SET contact_name = ? WHERE submitter_number = ? AND contact_user_number = ?",
                    name,
                    from,
                    num
                )
                .execute(pool)
                .await?;
                updated = true;
            }
        } else {
            new_numbers.push((num, desc));
        }
    }

    if new_numbers.is_empty() {
        return Ok(if updated {
            ImportResult::Updated
        } else {
            ImportResult::Unchanged
        });
    }

    if new_numbers.len() > 1 {
        // Store for later confirmation
        let deferred = DeferredContact {
            name: name.to_string(),
            numbers: new_numbers,
            timestamp: Instant::now(),
        };

        let mut deferred_contacts = DEFERRED_CONTACTS.lock().unwrap();
        deferred_contacts
            .entry(from.to_string())
            .or_default()
            .push(deferred);

        Ok(ImportResult::Deferred)
    } else {
        // Single number case - proceed with insertion
        let (number, _) = new_numbers.into_iter().next().unwrap();

        let mut tx = pool.begin().await?;

        // Create user if needed
        let contact_user = query!("SELECT * FROM users WHERE number = ?", number)
            .fetch_optional(&mut *tx)
            .await?;

        if contact_user.is_none() {
            query!(
                "INSERT INTO users (number, name) VALUES (?, ?)",
                number,
                name
            )
            .execute(&mut *tx)
            .await?;
        }

        // Insert contact
        query!(
            "INSERT INTO contacts (submitter_number, contact_name, contact_user_number) 
             VALUES (?, ?, ?)",
            from,
            name,
            number
        )
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(ImportResult::Added)
    }
}

async fn onboard_new_user(
    command: Option<Result<Command, serde_json::Error>>,
    words: impl Iterator<Item = &str>,
    from: &str,
    pool: &Pool<Sqlite>,
) -> anyhow::Result<String> {
    let Some(Ok(Command::name)) = command else {
        return Ok(format!(
            "Greetings! This is Decision Bot (https://github.com/samcarey/decisionbot).\n\
            To participate:\n{}",
            Command::name.hint()
        ));
    };
    Ok(match process_name(words) {
        Ok(name) => {
            query!("insert into users (number, name) values (?, ?)", from, name)
                .execute(pool)
                .await?;
            format!("Hello, {name}! {}", Command::h.hint())
        }
        Err(hint) => hint.to_string(),
    })
}

fn process_name<'a>(words: impl Iterator<Item = &'a str>) -> Result<String> {
    let name = words.collect::<Vec<_>>().join(" ");
    if name.is_empty() {
        bail!("{}", Command::name.usage());
    }
    const MAX_NAME_LEN: usize = 20;
    if name.len() > MAX_NAME_LEN {
        bail!(
            "That name is {} characters long.\n\
            Please shorten it to {MAX_NAME_LEN} characters or less.",
            name.len()
        );
    }
    Ok(name)
}

async fn send(twilio_config: &Configuration, to: String, message: String) -> Result<()> {
    let message_params = CreateMessageParams {
        account_sid: env::var("TWILIO_ACCOUNT_SID")?,
        to,
        from: Some(env::var("SERVER_NUMBER")?),
        body: Some(message),
        ..Default::default()
    };
    let message = create_message(twilio_config, message_params)
        .await
        .context("While sending message")?;
    trace!("Message sent with SID {}", message.sid.unwrap().unwrap());
    Ok(())
}
// Add these new types to the top of main.rs
#[derive(Debug, Clone)]
struct DeferredContact {
    name: String,
    numbers: Vec<(String, Option<String>)>, // (number, description) pairs
    timestamp: Instant,
}

static DEFERRED_CONTACTS: Lazy<Mutex<HashMap<String, Vec<DeferredContact>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

const DEFERRED_TIMEOUT: Duration = Duration::from_secs(300); // 5 minutes

// Update ImportStats to include deferred count
#[derive(Default)]
struct ImportStats {
    added: usize,
    updated: usize,
    skipped: usize,
    failed: usize,
    deferred: usize,
    errors: std::collections::HashMap<String, usize>,
}

impl ImportStats {
    fn add_error(&mut self, error: &str) {
        *self.errors.entry(error.to_string()).or_insert(0) += 1;
        self.failed += 1;
    }

    fn format_report(&self, from: &str) -> String {
        let mut report = format!(
            "Processed contacts: {} added, {} updated, {} unchanged, {} deferred, {} failed",
            self.added, self.updated, self.skipped, self.deferred, self.failed
        );

        if !self.errors.is_empty() {
            report.push_str("\nErrors encountered:");
            for (error, count) in &self.errors {
                report.push_str(&format!("\n- {} × {}", count, error));
            }
        }

        if self.deferred > 0 {
            // Add list of deferred contacts
            if let Ok(deferred_map) = DEFERRED_CONTACTS.lock() {
                if let Some(deferred) = deferred_map.get(from) {
                    report.push_str(
                        "\n\nThe following contacts have multiple numbers. \
                        Reply with \"pick NA, MB, ...\" \
                        where N and M are from the list of contacts below \
                        and A and B are the letters for the desired phone numbers for each.\n",
                    );
                    for (i, contact) in deferred.iter().enumerate() {
                        report.push_str(&format!("\n{}. {}", i + 1, contact.name));
                        for (j, (number, description)) in contact.numbers.iter().enumerate() {
                            let letter = (b'a' + j as u8) as char;
                            let desc = description.as_deref().unwrap_or("no description");
                            report.push_str(&format!("\n   {}. {} ({})", letter, number, desc));
                        }
                    }
                }
            }
        }
        report
    }
}

struct PendingDeletion {
    contact_id: i64,
    timestamp: Instant,
}

static PENDING_DELETIONS: Lazy<Mutex<HashMap<String, PendingDeletion>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

const DELETION_TIMEOUT: Duration = Duration::from_secs(300); // 5 minutes
