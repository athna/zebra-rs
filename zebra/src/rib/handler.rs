use ipnet::{IpNet, Ipv4Net};

use super::link::{link_show, LinkAddr};
use super::os::message::{OsAddr, OsChannel, OsLink, OsMessage, OsRoute};
use super::os::os_dump_spawn;
use super::Link;
use crate::config::{
    path_from_command, ConfigChannel, ConfigOp, ConfigRequest, DisplayRequest, ShowChannel,
};
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write;
use std::net::{IpAddr, Ipv4Addr};

type Callback = fn(&Rib, Vec<String>) -> String;

#[derive(Debug)]
pub struct Nexthop {
    nexthop: Ipv4Addr,
}

#[derive(Debug)]
pub struct RibEntry {
    selected: bool,
    preference: u32,
    tag: u32,
    color: Vec<String>,
    nexthops: Vec<Nexthop>,
    gateway: IpAddr,
}

impl RibEntry {
    pub fn new() -> Self {
        Self {
            selected: false,
            preference: 0,
            tag: 0,
            color: Vec::new(),
            nexthops: Vec::new(),
            gateway: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        }
    }
}

#[derive(Debug)]
pub struct Rib {
    // pub tx: UnboundedSender<String>,
    // pub rx: UnboundedReceiver<String>,
    pub cm: ConfigChannel,
    pub show: ShowChannel,
    pub os: OsChannel,
    pub links: BTreeMap<u32, Link>,
    pub rib: prefix_trie::PrefixMap<Ipv4Net, RibEntry>,
    pub callbacks: HashMap<String, Callback>,
}

pub fn rib_show(rib: &Rib, _args: Vec<String>) -> String {
    let mut buf = String::new();

    buf.push_str(
        r#"Codes: K - kernel, C - connected, S - static, R - RIP, B - BGP
       O - OSPF, IA - OSPF inter area
       N1 - OSPF NSSA external type 1, N2 - OSPF NSSA external type 2
       E1 - OSPF external type 1, E2 - OSPF external type 2
       i - IS-IS, L1 - IS-IS level-1, L2 - IS-IS level-2, ia - IS-IS inter area\n"#,
    );

    for (prefix, entry) in rib.rib.iter() {
        write!(buf, "K  {:?}     {:?}\n", prefix, entry.gateway).unwrap();
    }

    buf
}

pub fn link_addr_update(link: &mut Link, addr: LinkAddr) {
    if addr.is_v4() {
        for a in link.addr4.iter() {
            if a.addr == addr.addr {
                return;
            }
        }
        link.addr4.push(addr);
    } else {
        for a in link.addr6.iter() {
            if a.addr == addr.addr {
                return;
            }
        }
        link.addr6.push(addr);
    }
}

pub fn link_addr_del(link: &mut Link, addr: LinkAddr) {
    if addr.is_v4() {
        if let Some(remove_index) = link.addr4.iter().position(|x| x.addr == addr.addr) {
            link.addr4.remove(remove_index);
        }
    } else if let Some(remove_index) = link.addr6.iter().position(|x| x.addr == addr.addr) {
        link.addr6.remove(remove_index);
    }
}

impl Rib {
    pub fn new() -> Self {
        //let (tx, rx) = mpsc::unbounded_channel();
        let mut rib = Rib {
            //tx,
            //rx,
            cm: ConfigChannel::new(),
            show: ShowChannel::new(),
            os: OsChannel::new(),
            links: BTreeMap::new(),
            rib: prefix_trie::PrefixMap::new(),
            callbacks: HashMap::new(),
        };
        rib.callback_build();
        rib
    }

    pub fn link_by_name(&self, link_name: &str) -> Option<&Link> {
        self.links
            .iter()
            .find_map(|(_, v)| if v.name == link_name { Some(v) } else { None })
    }

    pub fn link_comps(&self) -> Vec<String> {
        self.links.values().map(|link| link.name.clone()).collect()
    }

    pub fn callback_add(&mut self, path: &str, cb: Callback) {
        self.callbacks.insert(path.to_string(), cb);
    }

    pub fn callback_build(&mut self) {
        self.callback_add("/show/interfaces", link_show);
        self.callback_add("/show/ip/route", rib_show);
    }
    pub fn link_add(&mut self, oslink: OsLink) {
        if !self.links.contains_key(&oslink.index) {
            let link = Link::from(oslink);
            self.links.insert(link.index, link);
        }
    }

    pub fn link_delete(&mut self, oslink: OsLink) {
        self.links.remove(&oslink.index);
    }

    pub fn addr_add(&mut self, osaddr: OsAddr) {
        let addr = LinkAddr::from(osaddr);
        if let Some(link) = self.links.get_mut(&addr.link_index) {
            link_addr_update(link, addr);
        }
    }

    pub fn addr_del(&mut self, osaddr: OsAddr) {
        let addr = LinkAddr::from(osaddr);
        if let Some(link) = self.links.get_mut(&addr.link_index) {
            link_addr_del(link, addr);
        }
    }

    pub fn route_add(&mut self, osroute: OsRoute) {
        if let IpNet::V4(v4) = osroute.route {
            let mut rib = RibEntry::new();
            rib.gateway = osroute.gateway;
            self.rib.insert(v4, rib);
        }
    }

    pub fn route_del(&mut self, _osroute: OsRoute) {
        //
    }

    fn process_os_message(&mut self, msg: OsMessage) {
        match msg {
            OsMessage::NewLink(link) => {
                self.link_add(link);
            }
            OsMessage::DelLink(link) => {
                self.link_delete(link);
            }
            OsMessage::NewAddr(addr) => {
                self.addr_add(addr);
            }
            OsMessage::DelAddr(addr) => {
                self.addr_del(addr);
            }
            OsMessage::NewRoute(route) => {
                self.route_add(route);
            }
            OsMessage::DelRoute(route) => {
                self.route_del(route);
            }
        }
    }

    fn process_cm_message(&self, msg: ConfigRequest) {
        if msg.op == ConfigOp::Completion {
            msg.resp.unwrap().send(self.link_comps()).unwrap();
        }
    }

    async fn process_show_message(&self, msg: DisplayRequest) {
        let (path, args) = path_from_command(&msg.paths);
        if let Some(f) = self.callbacks.get(&path) {
            let output = f(self, args);
            msg.resp.send(output).await.unwrap();
        }
    }

    pub async fn event_loop(&mut self) {
        os_dump_spawn(self.os.tx.clone()).await.unwrap();

        loop {
            tokio::select! {
                Some(msg) = self.os.rx.recv() => {
                    self.process_os_message(msg);
                }
                Some(msg) = self.cm.rx.recv() => {
                    self.process_cm_message(msg);
                }
                Some(msg) = self.show.rx.recv() => {
                    self.process_show_message(msg).await;
                }
            }
        }
    }
}

pub fn serve(mut rib: Rib) {
    tokio::spawn(async move {
        rib.event_loop().await;
    });
}
