#[allow(unused_unsafe)]
mod more_test;

fn main() {
  // let test_string = "fn main() {  unsafe {    { let x = 42; } }".to_string();
  // let mut start_of_unsafe_block = false;
  // let is_unsafe = contains_unsafe(test_string, &mut start_of_unsafe_block);
  let mut x = 0;

  very_safe_function();
  very_unsafe_function(&mut x);
  println!("x: {}", x);
  more_test::run();
}

fn very_safe_function() {
  let x = 42;
  _ = x+1; 
} 
 
 
fn very_unsafe_function(x: &mut i32) {
  for _ in 0..2 {
    unsafe {  
        // 1 
        *x = *x + 1;
    }
  }

  unsafe {
    // 2
    *x = *x + 1;
  }
}
 
#[allow(dead_code)]
fn allowed_whitespace(c: char, index: usize, indices: [usize; 2]) -> bool {
  for i in indices.iter() {
      if c.is_whitespace() && index == *i {
          return true;
      }
  }
  return false;
} 