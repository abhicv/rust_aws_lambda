use aws_config;
use aws_lambda_events::{s3::S3Event, s3::S3EventRecord};
use aws_sdk_s3::Client as S3Client;
use lambda_runtime::{run, service_fn, Error, LambdaEvent};

use serde_json::Value;
use lazy_static::lazy_static;

use std::sync::{Arc, Mutex};
use std::env;

mod s3;
use s3::{GetFile, PutFile};

use lettre::message::header::ContentType;
use lettre::message::SinglePart;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Message, SmtpTransport, Transport};
use tinytemplate::TinyTemplate;

#[derive(serde::Serialize, Clone)]
struct S3ObjectInfo {
    s3_uri: String,
    object_name: String,
    object_type: String,
    object_size: i64,
}

#[derive(serde::Serialize)]
struct Context {
    list: Vec<S3ObjectInfo>,
}

lazy_static! {
    static ref global_arc: Arc<Mutex<Vec<S3ObjectInfo>>> = Arc::new(Mutex::new(Vec::new()));
}

async fn function_handler(event: LambdaEvent<Value>, client: &S3Client) -> Result<(), Error> {
    let value = event.payload;

    if value.get("Records").is_some() {
        let s3_event: S3Event = serde_json::from_value(value).unwrap();
        let result = s3_event_handler(s3_event, client).await;
        return result;
    } else if value.get("time").is_some() {
        send_daily_report_mail();
    }

    Ok(())
}

async fn s3_event_handler(event: S3Event, client: &S3Client) -> Result<(), Error> {
    // println!("Event recevied: {:?}", event);

    let records = event.records;

    for record in records.into_iter() {
        let (bucket, key) = match get_file_props(record) {
            Ok(touple) => touple,
            Err(msg) => {
                tracing::info!("Record skipped with reason: {}", msg);
                continue;
            }
        };

        // let object_name = urlencoding::decode(key.as_str()).unwrap().to_string();
        let object_name = key.replace("+", " ");
        let s3_uri = "s3://".to_string() + bucket.as_str() + "/" + object_name.as_str();

        println!("{:?}", (bucket.to_string(), object_name.as_str()));

        let response = match client
            .get_object()
            .bucket(bucket.as_str())
            .key(object_name.as_str())
            .send()
            .await
        {
            Ok(reponse) => reponse,
            Err(msg) => {
                println!("Failed to get object head: {:?}", msg);
                continue;
            }
        };

        let object_type = response.content_type().unwrap();
        let object_size = response.content_length();

        println!(
            "{:?}",
            (
                s3_uri.to_owned(),
                object_name.as_str(),
                object_type,
                object_size
            )
        );

        {
            global_arc.lock().unwrap().push(S3ObjectInfo {
                s3_uri: s3_uri.to_string(),
                object_name: object_name.to_string(),
                object_type: object_type.to_string(),
                object_size: object_size,
            });
        }

        // avoiding recursive call
        if object_name.starts_with("thumbnail-") {
            return Ok(());
        }

        if object_type.starts_with("image") {
            println!("Image file upload!");

            let image = match client.get_file(&bucket, object_name.as_str()).await {
                Ok(vec) => vec,
                Err(msg) => {
                    tracing::info!("Can not get file from S3: {}", msg);
                    continue;
                }
            };

            println!("Creating thumbnail!");

            let thumbnail = match get_thumbnail(image, object_type, 128) {
                Ok(vec) => vec,
                Err(msg) => {
                    println!("Can not create thumbnail: {}", msg);
                    continue;
                }
            };

            let thumbnail_key = "thumbnail-".to_string() + object_name.as_str();
            println!("Thumbnail created: {}", thumbnail_key);

            match client.put_file(&bucket, &thumbnail_key, thumbnail).await {
                Ok(msg) => println!("{}", msg),
                Err(msg) => println!("Can not upload thumbnail: {}", msg),
            }
        }
    }

    Ok(())
}

fn get_file_props(record: S3EventRecord) -> Result<(String, String), String> {
    record
        .event_name
        .filter(|s| s.starts_with("ObjectCreated"))
        .ok_or("Wrong event")?;

    let bucket = record
        .s3
        .bucket
        .name
        .filter(|s| !s.is_empty())
        .ok_or("No bucket name")?;

    let key = record
        .s3
        .object
        .key
        .filter(|s| !s.is_empty())
        .ok_or("No object key")?;

    Ok((bucket, key))
}

fn get_thumbnail(vec: Vec<u8>, image_type: &str, size: u32) -> Result<Vec<u8>, String> {
    use std::io::Cursor;
    use thumbnailer::{create_thumbnails, ThumbnailSize};

    let reader = Cursor::new(vec);
    let mime: mime::Mime = image_type.parse().unwrap();

    let sizes = [ThumbnailSize::Custom((size, size))];

    let thumbnail = match create_thumbnails(reader, mime, sizes) {
        Ok(mut thumbnails) => thumbnails.pop().ok_or("No thumbnail created")?,
        Err(thumb_error) => return Err(thumb_error.to_string()),
    };

    let mut buf = Cursor::new(Vec::new());

    match thumbnail.write_png(&mut buf) {
        Ok(_) => Ok(buf.into_inner()),
        Err(_) => Err("Unknown error when Thumbnail::write_png".to_string()),
    }
}

fn send_daily_report_mail() {

    let context = Context {
        list: global_arc.lock().unwrap().clone(),
    };

    {
        // clearing the list
        global_arc.lock().unwrap().clear();
    }

    let email_body_html_template = r#"
    <!DOCTYPE html>
    <html>
        <head>
            <style> table, th, td \{
                border: 1px solid black;
                border-collapse: collapse;
            }
            th, td \{
                padding: 15px;
            }
            </style>
        </head>
        <body>
            <h1>Daily S3 upload report </h1>
            <table>
                <tr>
                    <th>Object Name</th>
                    <th>Object Type</th>
                    <th>Object Size</th>
                    <th>S3 URI</th>
                </tr> 
                {{for info in list}}
                <tr>
                    <td> {info.object_name} </td>
                    <td> {info.object_type} </td>
                    <td> {info.object_size} </td>
                    <td> {info.s3_uri} </td>
                </tr> 
                {{endfor}}
        </table>
    </body>
    </html>
    "#;

    let mut tt = TinyTemplate::new();
    tt.add_template("email_body", email_body_html_template)
        .unwrap();
    let rendered = tt.render("email_body", &context).unwrap();
    // println!("{}", rendered);

    let email = Message::builder()
        .from(
            "Abhijith C V <abhijithcheruvery@gmail.com>"
                .parse()
                .unwrap(),
        )
        .to("Abhijith C V <abhijithcheruvery@gmail.com>"
            .parse()
            .unwrap())
        .subject("Automated mail from Rust")
        // .header(ContentType::TEXT_PLAIN)
        .header(ContentType::TEXT_HTML)
        .singlepart(SinglePart::html(rendered))
        // .body(String::from("Hey, the mail client worked!"))
        .unwrap();
    
    let username = match env::var("EMAIL_USERNAME") {
        Ok(username) => username,
        Err(_) => {
            println!("Error: Environment variable EMAIL_USERNAME not set!");
            return;
        }
    };

    let password = match env::var("EMAIL_PASSWORD") {
        Ok(username) => username,
        Err(_) => {
            println!("Error: Environment variable EMAIL_USERNAME not set!");
            return;
        }
    };
    
    let creds = Credentials::new(username, password);

    // Open a remote connection to gmail
    let mailer = SmtpTransport::relay("smtp.gmail.com")
        .unwrap()
        .credentials(creds)
        .build();

    // Send the email
    match mailer.send(&email) {
        Ok(_) => println!("Email sent successfully!"),
        Err(e) => println!("Could not send email: {e:?}"),
    }
}

#[tokio::main]
async fn main() -> Result<(), Error> {

    // let sample = S3ObjectInfo {
    //     s3_uri: "s3://learns3bucket10/sequence.png".to_string(),
    //     object_name: "sequence.png".to_string(),
    //     object_type: "image/png".to_string(),
    //     object_size: 12345,
    // };

    // global_arc.lock().unwrap().push(sample);

    // NOTE: reads credential from /USER/.aws/credential file
    let config = aws_config::load_from_env().await;
    let client = S3Client::new(&config);
    let client_ref = &client;

    let run_result = run(service_fn(move |event| async move {
        function_handler(event, client_ref).await
    }))
    .await;

    Ok(())
}
