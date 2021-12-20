/*
 * paperback: paper backup generator suitable for long-term storage
 * Copyright (C) 2018-2020 Aleksa Sarai <cyphar@cyphar.com>
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program.  If not, see <https://www.gnu.org/licenses/>.
 */

mod raw;

use std::{
    error::Error as StdError,
    fs::File,
    io,
    io::{prelude::*, BufReader, BufWriter},
};

use anyhow::{anyhow, ensure, Context, Error};
use clap::{App, Arg, ArgMatches, SubCommand};

extern crate paperback_core;
use paperback_core::latest as paperback;

use paperback::{pdf::qr, wire, Backup, FromWire, KeyShardCodewords, ToPdf};

fn backup(matches: &ArgMatches<'_>) -> Result<(), Error> {
    let sealed: bool = matches
        .value_of("sealed")
        .expect("invalid --sealed argument")
        .parse()
        .context("--sealed argument was not a boolean")?;
    let quorum_size: u32 = matches
        .value_of("quorum_size")
        .expect("required --quorum_size argument not given")
        .parse()
        .context("--quorum-size argument was not an unsigned integer")?;
    let num_shards: u32 = matches
        .value_of("shards")
        .expect("required --shards argument not given")
        .parse()
        .context("--shards argument was not an unsigned integer")?;
    let input_path = matches
        .value_of("INPUT")
        .expect("required INPUT argument not given");

    let input: Box<dyn Read + 'static> = if input_path == "-" {
        Box::new(io::stdin())
    } else {
        Box::new(
            File::open(&input_path)
                .with_context(|| format!("failed to open secret data file '{}'", input_path))?,
        )
    };
    let mut buffer_input = BufReader::new(input);

    let mut secret = Vec::new();
    buffer_input
        .read_to_end(&mut secret)
        .with_context(|| format!("failed to read secret data from '{}'", input_path))?;

    let backup = if sealed {
        Backup::new_sealed(quorum_size, &secret)
    } else {
        Backup::new(quorum_size, &secret)
    }?;
    let main_document = backup.main_document().clone();
    let shards = (0..num_shards)
        .map(|_| backup.next_shard().unwrap())
        .map(|s| (s.id(), s.encrypt().unwrap()))
        .collect::<Vec<_>>();

    main_document
        .to_pdf()?
        .save(&mut BufWriter::new(File::create(format!(
            "main_document-{}.pdf",
            main_document.id()
        ))?))?;

    for (shard_id, (shard, codewords)) in shards {
        (shard, codewords)
            .to_pdf()?
            .save(&mut BufWriter::new(File::create(format!(
                "key_shard-{}-{}.pdf",
                main_document.id(),
                shard_id
            ))?))?;
    }

    Ok(())
}

fn read_multiline<S: AsRef<str>>(prompt: S) -> Result<String, Error> {
    print!("{}: ", prompt.as_ref());
    io::stdout().flush()?;

    let buffer_stdin = BufReader::new(io::stdin());
    Ok(buffer_stdin
        .lines()
        .take_while(|s| match s.as_deref() {
            Ok("") | Err(_) => false,
            _ => true,
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| anyhow!("failed to read data: {}", err))?
        .join("\n"))
}

fn read_multibase<S: AsRef<str>, T: FromWire>(prompt: S) -> Result<T, Error> {
    Ok(T::from_wire_multibase(
        wire::multibase_strip(read_multiline(prompt)?)
            .map_err(|err| anyhow!("failed to strip out non-multibase characters: {}", err))?,
    )
    .map_err(|err| anyhow!("failed to parse data: {}", err))?)
}

fn read_codewords<S: AsRef<str>>(prompt: S) -> Result<KeyShardCodewords, Error> {
    Ok(read_multiline(prompt)?
        .split_whitespace()
        .map(|s| s.to_owned())
        .collect::<Vec<_>>())
}

fn read_multibase_qr<S: AsRef<str>, T: FromWire>(prompt: S) -> Result<T, Error> {
    let prompt = prompt.as_ref();
    let mut joiner = qr::Joiner::new();
    while !joiner.complete() {
        let part: qr::Part = read_multibase(prompt)?;
        joiner.add_part(part)?;
    }
    T::from_wire(joiner.combine_parts()?)
        .map_err(|err| anyhow!("parse inner qr code data: {}", err))
}

fn recover(matches: &ArgMatches<'_>) -> Result<(), Error> {
    use paperback::{EncryptedKeyShard, MainDocument, UntrustedQuorum};

    let interactive: bool = matches
        .value_of("interactive")
        .expect("invalid --interactive argument")
        .parse()
        .context("--interactive argument was not a boolean")?;
    ensure!(interactive, "PDF scanning not yet implemented");
    let output_path = matches
        .value_of("OUTPUT")
        .expect("required OUTPUT argument not given");

    let main_document: MainDocument = read_multibase_qr("Main Document")?;
    let quorum_size = main_document.quorum_size();
    println!("Document ID: {}", main_document.id());
    // TODO: Ask the user to input the checksum...
    println!("Document Checksum: {}", main_document.checksum_string());

    let mut quorum = UntrustedQuorum::new();
    quorum.main_document(main_document);
    for idx in 0..quorum_size {
        let encrypted_shard: EncryptedKeyShard = read_multibase(format!("Shard {}", idx + 1))?;
        // TODO: Ask the user to input the checksum...
        println!(
            "Shard {} Checksum: {}",
            idx + 1,
            encrypted_shard.checksum_string()
        );

        let codewords = read_codewords(format!("Shard {} Codeword", idx + 1))?;
        let shard = encrypted_shard
            .decrypt(&codewords)
            .map_err(|err| anyhow!(err)) // TODO: Fix this once FromWire supports non-String errors.
            .with_context(|| format!("decrypting shard {}", idx + 1))?;

        println!("Loaded shard {}.", shard.id());
        quorum.push_shard(shard);
    }

    let quorum = match quorum.validate() {
        Ok(validated_quorum) => validated_quorum,
        Err(err) => {
            // TODO: Make this error much cleaner.
            return Err(anyhow!(
                "quorum failed to validate -- possible forgery! groupings: {:?}",
                err.as_groups()
            ));
        }
    };

    let secret = quorum
        .recover_document()
        .context("recovering secret data")?;

    let mut output_file: Box<dyn Write + 'static> =
        if output_path == "-" {
            Box::new(io::stdout())
        } else {
            Box::new(File::create(output_path).with_context(|| {
                format!("failed to open output file '{}' for writing", output_path)
            })?)
        };

    output_file
        .write_all(&secret)
        .context("write secret data to file")?;

    Ok(())
}

fn main() -> Result<(), Box<dyn StdError>> {
    let matches = App::new("paperback-cli")
        .version("0.0.0")
        .author("Aleksa Sarai <cyphar@cyphar.com>")
        .about("Operate on a paperback backup using a basic CLI interface.")
        // paperback-cli backup [--sealed] -n <QUORUM SIZE> -k <SHARDS> INPUT
        .subcommand(SubCommand::with_name("backup")
            .arg(Arg::with_name("sealed")
                .long("sealed")
                .help("Create a sealed backup, which cannot be expanded (have new shards be created) after creation.")
                .possible_values(&["true", "false"])
                .default_value("false"))
            .arg(Arg::with_name("quorum_size")
                .short("n")
                .long("quorum-size")
                .value_name("QUORUM SIZE")
                .help("Number of shards required to recover the document (must not be larger than --shards).")
                .takes_value(true)
                .required(true))
            .arg(Arg::with_name("shards")
                .short("k")
                .long("shards")
                .value_name("NUM SHARDS")
                .help("Number of shards to create (must not be smaller than --quorum-size).")
                .takes_value(true)
                .required(true))
            .arg(Arg::with_name("INPUT")
                .help(r#"Path to file containing secret data to backup ("-" to read from stdin)."#)
                .allow_hyphen_values(true)
                .required(true)
                .index(1)))
        // paperback-cli recover --interactive
        .subcommand(SubCommand::with_name("recover")
            .arg(Arg::with_name("interactive")
                .long("interactive")
                .help("Ask for data stored in QR codes interactively rather than scanning images.")
                .possible_values(&["true", "false"])
                .default_value("true"))
            .arg(Arg::with_name("OUTPUT")
                .help(r#"Path to write recovered secret data to ("-" to write to stdout)."#)
                .allow_hyphen_values(true)
                .required(true)
                .index(1)))
        .subcommand(raw::subcommands())
        .get_matches();

    let ret = match matches.subcommand() {
        ("raw", Some(sub_matches)) => raw::submatch(sub_matches),
        ("backup", Some(sub_matches)) => backup(sub_matches),
        ("recover", Some(sub_matches)) => recover(sub_matches),
        (subcommand, _) => Err(anyhow!("unknown subcommand '{}'", subcommand)),
    }?;

    Ok(ret)
}
