use error_chain::error_chain;

error_chain! {
    foreign_links {
        Io(std::io::Error);
        Tunstenite(tungstenite::error::Error);
        Mpsc(futures::channel::mpsc::SendError);
    }
}
