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
    Input::File(file_path) => {
      // borrow file_path
      let contents = annotate_unsafe_file(&file_path);
      Input::Str { name: file_path.into(), input: contents }
    },
    Input::Str { name, input } => Input::Str { name, input: annotate_unsafe(input) },
  };
}

// check if a line contains "unsafe {"
pub fn contains_unsafe(input: String) -> bool {
  let query = " unsafe { ";
  let mut query_index = 0;
  for c in input.chars() {
    let current = query.chars().nth(query_index).unwrap();
    if c == current || (current.is_whitespace() && c.is_whitespace()) {
      query_index += 1;
      if query_index == query.len() {
        return true;
      }
    } else {
      query_index = 0;
    }
  }
  return false;
}

const PING_FUNCTION: &str = "pub fn ping() {
  let mut stream = match std::net::TcpStream::connect(\"127.0.0.1:7910\") {
    Ok(stream) => stream,
    Err(_) => return,
  };
  std::io::Write::write(&mut stream, &[1]);
  return
}";

fn annotate_unsafe(input: String) -> String { 
  let input_vec: Vec<&str> = input.split('\n').collect();
  let mut in_unsafe_block = false;
  let mut file_buffer = Vec::<String>::new();
  let mut unsafe_vec = Vec::<String>::new(); // unsafe vec will be a back-stack, popping and pushing from the back
  for line in input_vec {
    if contains_unsafe(line.to_string()) || in_unsafe_block && !line.trim().is_empty() {
      // push every { and } to a vector
      for c in line.chars() {
        if c == '{' {
            unsafe_vec.push(c.to_string());
        } else if c == '}' {
            unsafe_vec.pop();
        }
      }
      // if the vector is empty, we are out of the unsafe block
      if unsafe_vec.is_empty() {
          in_unsafe_block = false;
      }
      file_buffer.push(line.to_string());
      // if there is no ';' in the line, cannot be a ping target
      if line.contains(';') {
          file_buffer.push("ping();".to_string());
      }
    } else {
        file_buffer.push(line.to_string());
    }
  }
  // add ping function to the start of content
  file_buffer.insert(0, PING_FUNCTION.to_string());
  return file_buffer.join("\n");
}

fn annotate_unsafe_file(file_path: &PathBuf) -> String {
  let mut file = File::open(file_path).unwrap();
  let mut buffer = Vec::new();
  file.read_to_end(&mut buffer).unwrap();
  return annotate_unsafe(buffer.iter().map(|&c| c as char).collect());
}