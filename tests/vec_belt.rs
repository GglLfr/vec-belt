use std::{iter::repeat_with, sync::Arc};

use vec_belt::VecBelt;

#[derive(Debug, PartialEq)]
struct Int(usize);

impl Drop for Int {
    fn drop(&mut self) {
        println!("Dropped {}", self.0);
    }
}

#[test]
fn test_vec_belt() {
    let append = [0, 1, 2, 3, 4];
    let thread_count = 8;

    let vec = Arc::new(VecBelt::new(1));
    let threads = (0..thread_count)
        .zip(repeat_with(|| vec.clone()))
        .map(|(i, vec)| std::thread::spawn(move || vec.append(append.map(|num| Int(num + i * append.len())))))
        .collect::<Box<_>>();

    for thread in threads {
        thread.join().unwrap();
    }

    Arc::into_inner(vec).unwrap().clear(|slice| {
        assert_eq!(slice.len(), append.len() * thread_count);
        for i in 0..thread_count {
            let slice = &slice[i * append.len()..(i + 1) * append.len()];
            for (j, num) in slice.iter().enumerate() {
                assert_eq!(num.0, append[j] + slice[0].0);
            }
        }
    })
}
