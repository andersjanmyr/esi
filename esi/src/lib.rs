mod config;
mod parse;

pub use crate::config::Configuration;
use crate::parse::{parse_tags, Event, Tag};
use fastly::http::body::StreamingBody;
use fastly::http::header;
use fastly::http::request::SendError;
use fastly::{Body, Request, Response};
use log::{debug, error, warn};
use quick_xml::{Reader, Writer};
use std::io::Write;
use thiserror::Error;

#[derive(Error, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum ExecutionError {
    #[error("xml parsing error: {0}")]
    XMLError(#[from] quick_xml::Error),
    #[error("tag `{0}` is missing required parameter `{1}`")]
    MissingRequiredParameter(String, String),
    #[error("unexpected `{0}` closing tag")]
    UnexpectedClosingTag(String),
    #[error("duplicate attribute detected: {0}")]
    DuplicateTagAttribute(String),
    #[error("error sending request: {0}")]
    RequestError(#[from] SendError),
    #[error("received unexpected status code for fragment: {0}")]
    UnexpectedStatus(u16),
    #[error("unknown error")]
    Unknown,
}

pub type Result<T> = std::result::Result<T, ExecutionError>;

#[derive(Default)]
pub struct Processor {
    configuration: Configuration,
}

impl Processor {
    pub fn new(configuration: Configuration) -> Self {
        Self { configuration }
    }
}

impl Processor {
    pub fn execute_esi(
        &self,
        original_request: Request,
        mut document: Response,
        request_handler: &dyn Fn(Request) -> Result<Response>,
    ) -> Result<()> {
        // Create a parser for the ESI document
        let body = document.take_body();
        let xml_reader = Reader::from_reader(body);

        // Send the response headers to the client and open an output stream
        let output = document.stream_to_client();

        // Set up an XML writer to write directly to the client output stream.
        let mut xml_writer = Writer::new(output);

        // Parse the ESI document
        match self.execute_esi_fragment(
            original_request,
            xml_reader,
            &mut xml_writer,
            request_handler,
        ) {
            Ok(_) => Ok(()),
            Err(err) => {
                error!("error executing ESI: {:?}", err);
                xml_writer.write(b"\nAn error occurred while constructing this document.\n")?;
                xml_writer
                    .inner()
                    .flush()
                    .expect("failed to flush error message");
                Err(err)
            }
        }
    }

    pub fn execute_esi_fragment(
        &self,
        original_request: Request,
        mut xml_reader: Reader<Body>,
        xml_writer: &mut Writer<StreamingBody>,
        request_handler: &dyn Fn(Request) -> Result<Response>,
    ) -> Result<()> {
        // Parse the ESI fragment
        parse_tags(
            &self.configuration.namespace,
            &mut xml_reader,
            &mut |event| {
                match event {
                    Event::ESI(Tag::Include {
                        src,
                        alt,
                        continue_on_error,
                    }) => {
                        let resp = match self.send_esi_fragment_request(
                            &original_request,
                            &src,
                            request_handler,
                        ) {
                            Ok(resp) => Some(resp),
                            Err(err) => {
                                warn!("Request to {} failed: {:?}", src, err);
                                if let Some(alt) = alt {
                                    warn!("Trying `alt` instead: {}", alt);
                                    match self.send_esi_fragment_request(
                                        &original_request,
                                        &alt,
                                        request_handler,
                                    ) {
                                        Ok(resp) => Some(resp),
                                        Err(err) => {
                                            debug!("Alt request to {} failed: {:?}", alt, err);
                                            if continue_on_error {
                                                None
                                            } else {
                                                return Err(err);
                                            }
                                        }
                                    }
                                } else {
                                    error!("Fragment request failed with no `alt` available");
                                    if continue_on_error {
                                        None
                                    } else {
                                        return Err(err);
                                    }
                                }
                            }
                        };

                        if let Some(mut resp) = resp {
                            let reader = Reader::from_reader(resp.take_body());
                            self.execute_esi_fragment(
                                original_request.clone_without_body(),
                                reader,
                                xml_writer,
                                request_handler,
                            )?;
                        } else {
                            error!("No content for fragment");
                        }
                    }
                    Event::XML(event) => {
                        xml_writer.write_event(event)?;
                        xml_writer.inner().flush().expect("failed to flush output");
                    }
                }
                Ok(())
            },
        )?;

        Ok(())
    }

    fn send_esi_fragment_request(
        &self,
        original_request: &Request,
        url: &str,
        request_handler: &dyn Fn(Request) -> Result<Response>,
    ) -> Result<Response> {
        let mut req = original_request
            .clone_without_body()
            .with_url(url)
            .with_pass(true);

        let hostname = req.get_url().host().expect("no host").to_string();

        req.set_header(header::HOST, &hostname);

        debug!("Requesting ESI fragment: {}", url);

        let resp = request_handler(req)?;
        if resp.get_status().is_success() {
            Ok(resp)
        } else {
            Err(ExecutionError::UnexpectedStatus(resp.get_status().as_u16()))
        }
    }
}
