use aws_lambda_events::{s3::S3Event, s3::S3EventRecord};
use aws_sdk_dynamodb::types::AttributeValue;
use aws_sdk_dynamodb::Client as DynamoDBClient;
use aws_sdk_s3::Client as S3Client;
use lambda_runtime::{run, service_fn, Error, LambdaEvent};

use serde_json::Value;

use std::collections::HashMap;
use std::env;

mod s3;
use s3::{GetFile, PutFile};

use lettre::message::header::ContentType;
use lettre::message::SinglePart;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Message, SmtpTransport, Transport};
use tinytemplate::TinyTemplate;

#[derive(serde::Serialize, Clone, Default, Debug)]
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

// handler for lambda function
async fn function_handler(
    event: LambdaEvent<Value>,
    s3_client: &S3Client,
    db_client: &DynamoDBClient,
) -> Result<(), Error> {
    let value = event.payload;

    if value.get("Records").is_some() {
        let s3_event: S3Event = serde_json::from_value(value).unwrap();
        let result = s3_event_handler(s3_event, s3_client, db_client).await;
        return result;
    } else if value.get("time").is_some() {
        send_daily_report_mail(db_client).await;
    }

    Ok(())
}

async fn s3_event_handler(
    event: S3Event,
    s3_client: &S3Client,
    db_client: &DynamoDBClient,
) -> Result<(), Error> {
    let records = event.records;

    for record in records.into_iter() {
        let (bucket, key) = match get_file_props(record) {
            Ok(touple) => touple,
            Err(msg) => {
                println!("Record skipped with reason: {}", msg);
                continue;
            }
        };

        let object_name = key.replace("+", " ");
        let s3_uri = "s3://".to_string() + bucket.as_str() + "/" + object_name.as_str();

        println!("{:?}", (bucket.to_string(), object_name.as_str()));

        let response = match s3_client
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

        let s3_info = S3ObjectInfo {
            s3_uri: s3_uri.to_string(),
            object_name: object_name.to_string(),
            object_type: object_type.to_string(),
            object_size: object_size.unwrap(),
        };

        match put_s3_info_in_db(db_client, &s3_info).await {
            Ok(_) => {},
            Err(error) => {
                println!("Failed to dump s3 info into DB: {:?}", error);
            }
        }

        // avoiding recursive call
        if object_name.starts_with("thumbnail-") {
            return Ok(());
        }

        if object_type.starts_with("image") {
            println!("Image file upload!");

            let supported_image_formats = vec!["image/png", "image/jpeg", "image/jpg"];

            if !supported_image_formats.contains(&object_type) {
                println!("Unsupported image format, skipping thumbnail creation");
                continue;
            }

            let image = match s3_client.get_file(&bucket, object_name.as_str()).await {
                Ok(vec) => vec,
                Err(msg) => {
                    println!("Can not get file from S3: {}", msg);
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

            match s3_client.put_file(&bucket, &thumbnail_key, thumbnail).await {
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

async fn send_daily_report_mail(db_client: &DynamoDBClient) {

    let context = Context {
        list: match get_s3_info_from_db(db_client).await {
            Ok(value) => value,
            Err(_) => return,
        },
    };

    // deleting dynamodb table contents
    match delete_s3_info_from_db(db_client, &context.list).await {
        Ok(_) => {},
        Err(error) => {
            println!("Failed to delete s3 object info items from DB: {:?}", error);
        }
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

fn convert_s3_info_into_attribute_map(s3_info: &S3ObjectInfo) -> HashMap<String, AttributeValue> {
    let mut result = HashMap::new();
    result.insert(
        String::from("s3_uri"),
        AttributeValue::S(s3_info.s3_uri.to_owned()),
    );
    result.insert(
        String::from("object_name"),
        AttributeValue::S(s3_info.object_name.to_owned()),
    );
    result.insert(
        String::from("object_type"),
        AttributeValue::S(s3_info.object_type.to_owned()),
    );
    result.insert(
        String::from("object_size"),
        AttributeValue::N(s3_info.object_size.to_string()),
    );
    return result;
}

fn convert_attribute_map_into_s3_info(
    attribute_map: &HashMap<String, AttributeValue>,
) -> Option<S3ObjectInfo> {
    let s3_info = S3ObjectInfo {
        s3_uri: attribute_map
            .get("s3_uri")
            .unwrap()
            .as_s()
            .unwrap()
            .to_string(),
        object_name: attribute_map
            .get("object_name")
            .unwrap()
            .as_s()
            .unwrap()
            .to_string(),
        object_type: attribute_map
            .get("object_type")
            .unwrap()
            .as_s()
            .unwrap()
            .to_string(),
        object_size: attribute_map
            .get("object_size")
            .unwrap()
            .as_n()
            .unwrap()
            .parse()
            .unwrap(),
    };
    return Some(s3_info);
}

async fn put_s3_info_in_db(db_client: &DynamoDBClient, s3_info: &S3ObjectInfo) -> Result<(), Error> {
    db_client
        .put_item()
        .table_name("object_uploads")
        .set_item(Some(convert_s3_info_into_attribute_map(s3_info)))
        .item(
            "this_is_a_partition_key",
            AttributeValue::S("123456".to_string()),
        )
        .item("a_sort_key", AttributeValue::S(s3_info.s3_uri.to_string()))
        .send().await?;
    Ok(())
}

async fn get_s3_info_from_db(db_client: &DynamoDBClient) -> Result<Vec<S3ObjectInfo>, Error> {
    match db_client
        .query()
        .table_name("object_uploads")
        .key_condition_expression("this_is_a_partition_key = :partition_key")
        .expression_attribute_values(":partition_key", AttributeValue::S("123456".to_string()))
        .send()
        .await
    {
        Ok(result) => {
            println!("{:?}", result.items());
            Ok(result
                .items()
                .to_vec()
                .iter()
                .map(|attribute_map| convert_attribute_map_into_s3_info(attribute_map).unwrap())
                .collect())
        }
        Err(error) => Err(Box::new(error))
    }
}

async fn delete_s3_info_from_db(db_client: &DynamoDBClient, s3_infos: &Vec<S3ObjectInfo>) -> Result<(), Error> {
    for info in s3_infos.iter() {
        db_client
            .delete_item()
            .table_name("object_uploads")
            .key(
                "this_is_a_partition_key",
                AttributeValue::S("123456".to_string()),
            )
            .key("a_sort_key", AttributeValue::S(info.s3_uri.to_owned()))
            .send()
            .await?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Error> {

    // NOTE: reads credential from /USER/.aws/credential file
    let config = aws_config::load_from_env().await;
    let s3_client = S3Client::new(&config);
    let s3_client_ref = &s3_client;

    let db_client = DynamoDBClient::new(&config);
    let db_client_ref = &db_client;

    run(service_fn(move |event| async move {
        function_handler(event, s3_client_ref, db_client_ref).await
    }))
    .await
}
