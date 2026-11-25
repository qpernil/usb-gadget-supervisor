use crate::{
    simulated::{File, gpio},
    wire::*,
};
use std::{
    io::{self, Read, Write},
    os::fd::OwnedFd,
};
use std::{os::unix::net::UnixDatagram, time::Duration};
#[test]
fn compatible_file_preserves_transfers_zlps_and_cancellation() {
    let (host, worker) = UnixDatagram::pair().unwrap();
    initialize(&host, HandleKind::Endpoint).unwrap();
    event(&host, true).unwrap();
    let mut file = File::from(OwnedFd::from(worker));
    send_transfer(&host, &[1, 2, 3, 4]).unwrap();
    send_transfer(&host, &[]).unwrap();
    let mut bytes = [0; 2];
    assert_eq!(file.read(&mut bytes).unwrap(), 2);
    assert_eq!(bytes, [1, 2]);
    assert_eq!(file.read(&mut bytes).unwrap(), 2);
    assert_eq!(bytes, [3, 4]);
    assert_eq!(file.read(&mut bytes).unwrap(), 0);
    let (sender, receiver) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        sender
            .send(file.read(&mut [0; 64]).unwrap_err().raw_os_error())
            .unwrap()
    });
    event(&host, false).unwrap();
    assert_eq!(
        receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
        Some(libc::ENODEV)
    );
    thread.join().unwrap();
}
#[test]
fn disable_cancels_a_backpressured_writer() {
    let (host, worker) = UnixDatagram::pair().unwrap();
    initialize(&host, HandleKind::Endpoint).unwrap();
    event(&host, true).unwrap();
    let mut packet = vec![0; 1024];
    packet[0] = DATA;
    loop {
        match send_packet(&worker, &packet, libc::MSG_DONTWAIT) {
            Ok(()) => {}
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock
                    || error.raw_os_error() == Some(libc::ENOBUFS) =>
            {
                break;
            }
            Err(error) => panic!("fill endpoint queue: {error}"),
        }
    }
    let mut file = File::from(OwnedFd::from(worker));
    let (sender, receiver) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        sender
            .send(file.write(&[1; 1023]).unwrap_err().raw_os_error())
            .unwrap()
    });
    assert!(matches!(
        receiver.recv_timeout(Duration::from_millis(20)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ));
    event(&host, false).unwrap();
    assert_eq!(
        receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
        Some(libc::ENODEV)
    );
    thread.join().unwrap();
}

#[test]
fn gpio_api_samples_shared_input_level_without_automatic_touch() {
    let (host, worker) = UnixDatagram::pair().unwrap();
    initialize(&host, HandleKind::DigitalInput).unwrap();
    let mut file = File::from(OwnedFd::from(worker));
    let sensor = file.try_clone().unwrap();
    let mut values = gpio::v2::LineValues { bits: 0, mask: 1 };
    gpio::v2::get_line_values(&sensor, &mut values).unwrap();
    assert_eq!(values.get(0), Some(false));
    send_transfer(&host, &[1]).unwrap();
    assert_eq!(file.read(&mut [0; 48]).unwrap(), 48);
    gpio::v2::get_line_values(&sensor, &mut values).unwrap();
    assert_eq!(values.get(0), Some(true));
}
