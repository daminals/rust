// run the unsafe parser preprocessor on the input string
// TODO: TURN THIS INTO A PREPROCESSOR MODULE
use rustc_session::config::Input;
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;

// strategy: convert all file inputs into string inputs, modify string inputs directly
// this will probably result in heavy latency
pub fn process_unsafe_input(input: Input) -> Input {
    return match input {
        Input::File(ref file_path) => {
            let input = file_to_str(file_path);
            let file_name = PathBuf::from(file_path);
            Input::Str { name: file_name.into(), input: annotate_unsafe(input) }
        }
        Input::Str { name, input } => Input::Str { name, input: annotate_unsafe(input) },
    };
}

fn file_to_str(file_path: &PathBuf) -> String {
    let mut file = File::open(file_path).unwrap();
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer).unwrap();
    let content: String = buffer.iter().map(|&c| c as char).collect();
    return content;
}

// check if a line contains "unsafe {" by utilizing custom regex matching
fn contains_unsafe(input: String, is_unsafe_block: &mut bool) -> bool {
    let query = " unsafe { ";
    let mut query_index = 0;
    for c in input.chars() {
        let current = query.chars().nth(query_index).unwrap();
        if c == current || (current.is_whitespace() && c.is_whitespace()) {
            query_index += 1;
            if query_index == query.len() {
                *is_unsafe_block = true;
                return true;
            }
        } else {
            query_index = 0;
        }
    }
    *is_unsafe_block = false;
    return false;
}

#[allow(dead_code)]
const PING_FUNCTION: &str = "pub fn ping() {
  let mut stream = match std::net::TcpStream::connect(\"127.0.0.1:7910\") {
    Ok(stream) => stream,
    Err(_) => return,
  };
  std::io::Write::write(&mut stream, &[1]);
  return
}";

// utilize bytes instead of chars so that it compile utf-8
fn split_by_newline(input: String) -> Vec<String> {
    let mut buf = Vec::<String>::new();
    let mut start = 0;
    for (i, &byte) in input.as_bytes().iter().enumerate() {
        if byte == b'\n' {
            buf.push(input[start..i].to_string());
            start = i + 1;
        }
    }
    if start < input.len() {
        buf.push(input[start..].to_string());
    }
    buf
}

// manual join
fn join_by_newline(input: Vec<String>) -> String {
    let mut buf = String::new();
    for line in input {
        buf.push_str(&line);
        buf.push('\n');
    }
    return buf;
}

// this will add special annotations to unsafe code in rust
// so that we can make calls to qemu
fn annotate_unsafe(input: String) -> String {
    let input_vec = split_by_newline(input);
    let mut in_unsafe_block = false;
    let mut start_of_unsafe = false;
    let mut file_buffer = Vec::<String>::new();
    let mut unsafe_vec = Vec::<char>::new(); // unsafe vec will be a back-stack, popping and pushing from the back
    for line in input_vec {
        file_buffer.push(line.clone());
        if !line.trim().is_empty()
            && (in_unsafe_block || contains_unsafe(line.to_string(), &mut start_of_unsafe))
        {
            for byte in line.bytes() {
                if start_of_unsafe {
                    // this is the first line of the unsafe block
                    // add something here to track unsafe entrance
                }

                // push every { and } to a vector
                match byte {
                    b'{' => {
                        unsafe_vec.push(byte as char);
                    }
                    // TODO: this is a potentially unsafe operation if a } is found without a
                    // { or if either is in a string of some sort
                    b'}' => {
                        unsafe_vec.pop();
                    }
                    _ => (),
                };
                // if the vector is empty, we are out of the unsafe block
                if unsafe_vec.is_empty() {
                    in_unsafe_block = false;
                    // this is the last line of the unsafe block
                    // add something here to track unsafe exit
                }
            }
        }
    }

    let join = join_by_newline(file_buffer);
    return join;
}
