//! Screen-scoped inventory subscriptions, reserving all existing conversation and terminal slots.
use st3_client::{ClientError, CollectionStream};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Inventory {
    Clients,
    Usage(u64),
    Machines,
    Devices,
    Sessions,
    Terminals,
}

impl Inventory {
    pub fn collection(self) -> &'static str {
        match self {
            Self::Clients => "clients",
            Self::Usage(_) => "usage",
            Self::Machines => "machines",
            Self::Devices => "devices",
            Self::Sessions => "sessions",
            Self::Terminals => "terminals",
        }
    }
    pub fn id(self) -> String {
        match self {
            Self::Usage(hours) => format!("usage-{hours}"),
            _ => self.collection().to_owned(),
        }
    }
    pub(crate) fn from_id(id: &str) -> Option<Self> {
        Some(match id {
            "clients" => Self::Clients,
            "machines" => Self::Machines,
            "devices" => Self::Devices,
            "sessions" => Self::Sessions,
            "terminals" => Self::Terminals,
            _ => Self::Usage(id.strip_prefix("usage-")?.parse().ok()?),
        })
    }
    pub(crate) async fn subscribe(self, stream: &mut CollectionStream) -> Result<(), ClientError> {
        match self {
            Self::Usage(hours) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                stream
                    .subscribe_usage(
                        &self.id(),
                        super::WINDOW,
                        Some(now.saturating_sub(hours.saturating_mul(3_600_000))),
                        None,
                    )
                    .await
            }
            Self::Devices => {
                stream
                    .subscribe_devices(&self.id(), super::WINDOW, None)
                    .await
            }
            _ => {
                stream
                    .subscribe(&self.id(), self.collection(), super::WINDOW, None, None, None)
                    .await
            }
        }
    }
}

/// One selected usage period plus five lists: at most six slots alongside the existing ten.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Inventories {
    pub clients: bool,
    pub usage_hours: Option<u64>,
    pub machines: bool,
    pub devices: bool,
    pub sessions: bool,
    pub terminals: bool,
}
impl Inventories {
    pub fn windows(self) -> Vec<Inventory> {
        [
            self.clients.then_some(Inventory::Clients),
            self.usage_hours.map(Inventory::Usage),
            self.machines.then_some(Inventory::Machines),
            self.devices.then_some(Inventory::Devices),
            self.sessions.then_some(Inventory::Sessions),
            self.terminals.then_some(Inventory::Terminals),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
    pub(crate) fn contains(self, inventory: Inventory) -> bool {
        self.windows().contains(&inventory)
    }
}
