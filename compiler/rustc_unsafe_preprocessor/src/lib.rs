// run the unsafe parser preprocessor on the input string
// TODO: TURN THIS INTO A PREPROCESSOR MODULE
use rustc_session::config::Input;
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;

const DEBUG: bool = true;
use std::io::{self, Write};
fn debug_print(input: String) {
    match io::stdout().write_all(&input.clone().into_bytes()) {
        Ok(_) => (),
        Err(_e) => (),
    }
}

#[allow(dead_code)]
enum InstrumentationCode {
  Asm,
  TimeIt,
}

impl InstrumentationCode {
    fn init(&self) -> String {
        match self {
            InstrumentationCode::Asm => {
                return String::from("global_asm!(r#\".globl unsafe_test\nunsafe_test:\nret\"#);");
            }
            InstrumentationCode::TimeIt => {
                return String::from("");
            }
        }
    }

    fn import(&self) -> String {
        match self {
            InstrumentationCode::Asm => {
                return String::from("use std::arch::{asm, global_asm};");
            }
            InstrumentationCode::TimeIt => {
                return String::from("");
            }
        }
    }

    fn start_unsafe(&self) -> String {
        match self {
            InstrumentationCode::Asm => {
                return String::from("asm!(\"call unsafe_test\");");
            }
            InstrumentationCode::TimeIt => {
                return String::from(r#"
                let pre_9830745908903894709234 = core::arch::x86_64::_rdtsc();
                "#);
            }
        }
    }

    fn end_unsafe(&self) -> String {
        match self {
            InstrumentationCode::Asm => {
                return self.start_unsafe();
            }
            InstrumentationCode::TimeIt => {
              return String::from(r#"
    let mut file = std::fs::OpenOptions::new()
    .read(false)
    .append(true)
    .create(true)
    .open("unsafe_times.txt")
    .expect("unable to open file");
    let output = format!("{}\n", core::arch::x86_64::_rdtsc() - pre_9830745908903894709234);
    std::io::Write::write_all(&mut file, output.as_bytes())
    .expect("unable to write to file");"#);
            }
        }
    }

}

// read environment variables to check if we are in a compiler environment
fn is_compiler() -> bool {
    // check if the environment variable RUSTC_COMPILATION is set
    // if it is set, do not inject code 
    // only tracked code will be desired program, not stdlib or compiler code
    return std::env::var("RUSTC_COMPILATION").is_ok();
}

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

// check if the character is allowed to be whitespace, and if it is a whitespace
fn allowed_whitespace(c: char, index: usize, indices: [usize; 2]) -> bool {
    for i in indices.iter() {
        if c.is_whitespace() && index == *i {
            return true;
        }
    }
    return false;
}
    
// check if a line contains "unsafe {" by utilizing custom regex matching
fn contains_unsafe(input: String, start_of_unsafe_block: &mut bool) -> bool {
    let query = " unsafe{";
    let mut query_index = 0;
    let mut prev_char = ' ';
    let input_chars = input.chars().collect::<Vec<char>>();
    let mut index = 0;
    while index < input_chars.len() {
        let c = input_chars[index];

        // IGNORE COMMENTS
        if c == '/' && prev_char == '/' {
            // move cursor to the next line
            while index < input_chars.len() && input_chars[index] != '\n' {
                index += 1;
            }
            continue;
        }

        let current = query.chars().nth(query_index).unwrap();
        if c == current || (current.is_whitespace() && c.is_whitespace()) {
            query_index += 1;
            if query_index == query.len() - 1 {
                *start_of_unsafe_block = true;
                // debug_print("unsafe block found\n".to_string());
                return true;
            }
        } else {
            // the allowed indices should be
            // 1: 'u' because it is the first character of the query, there can be infinite whitespace before it
            // 7: '{' because it is the last character of the query there can be infinite whitespace before it
            if !allowed_whitespace(c, query_index, [1, 7]) {
                query_index = 0;
            }
        }
        prev_char = c;
        index += 1;
    }
    *start_of_unsafe_block = false;
    return false;
}

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
    // check if we are in a compiler environment
    if is_compiler() {
        return input;
    }

    // check if the input is empty
    if input.trim().is_empty() {
        return input;
    }
    
    let input_vec = split_by_newline(input);
    let mut in_unsafe_block = false;
    let mut start_of_unsafe = false;
    let mut file_buffer = Vec::<String>::new();
    let mut unsafe_vec = Vec::<char>::new(); // unsafe vec will be a back-stack, popping and pushing from the back
    let mut prev_char = ' ';
    let mut in_string = false;
    let mut modified = false;

    let instrumenter = InstrumentationCode::Asm;
    
    file_buffer.push(instrumenter.import());
    file_buffer.push(instrumenter.init());

    for line in input_vec {
        file_buffer.push(line.clone());
        if !line.trim().is_empty()
            && (in_unsafe_block || contains_unsafe(line.to_string(), &mut start_of_unsafe))
        {
            if start_of_unsafe {
                modified = true;
                // this is the first line of the unsafe block
                // add something here to track unsafe entrance
                file_buffer.push(instrumenter.start_unsafe());
                start_of_unsafe = false;
            }
            in_unsafe_block = true;
            for byte in line.bytes() {
                // push every { and } to a vector
                match byte {
                    // IGNORE QUOTES
                    b'"' | b'\'' => {
                        if !(prev_char == '\\') {
                            in_string = !in_string;
                        }
                    }
                    b'/' => {
                        if prev_char == '/' {
                            break;
                        }
                    }
                    b'{' => {
                        if !in_string {
                            unsafe_vec.push(byte as char);
                        }
                    }
                    b'}' => {
                        if !in_string {
                            unsafe_vec.pop();
                        }
                    }
                    _ => (),
                };
                prev_char = byte as char;
            }
            // if the vector is empty, we are out of the unsafe block
            if unsafe_vec.is_empty() {
                in_unsafe_block = false;
                // this is the last line of the unsafe block
                // add something here to track unsafe exit
                // insert before last line (so that inline assembly is inside unsafe block)
                file_buffer.insert(file_buffer.len() - 1, instrumenter.end_unsafe());
            }
        }
    }

    // if the set is empty, remove the asm macro import
    if !modified {
        file_buffer.remove(0);
    }

    let join = join_by_newline(file_buffer);
    if DEBUG {
        debug_print(join.clone());
    }

    return join;
}