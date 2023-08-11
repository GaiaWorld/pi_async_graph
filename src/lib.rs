//! 异步执行 静态有向无环图 的运行节点
#![feature(associated_type_bounds)]
#![feature(test)]
extern crate test;

use flume::{bounded, Receiver, Sender};
use pi_async_rt::prelude::AsyncRuntime;
use pi_futures::BoxFuture;
use pi_graph::{DirectedGraph, DirectedGraphNode};
use pi_share::{ThreadSend, ThreadSync};
use std::io::{Error, ErrorKind, Result};
use std::marker::PhantomData;
use std::mem::transmute;
use std::sync::atomic::{AtomicUsize, Ordering};
use pi_slotmap::Key;

/// 同步执行节点
pub trait Runner<K: 'static + ThreadSync, Context: 'static + ThreadSync> {
    fn run(self, context: &'static Context, id: K, from: &[K], to: &[K]);
}

/// 可运行节点
pub trait Runnble<K: 'static + ThreadSync, Context: 'static + ThreadSync> {
    type R: Runner<K, Context> + ThreadSend + 'static;

    /// 判断是否同步运行， None表示不是可运行节点，true表示同步运行， false表示异步运行
    fn is_sync(&self) -> Option<bool>;
    /// 获得需要执行的同步函数
    fn get_sync(&self) -> Self::R;
    /// 获得需要执行的异步块
    fn get_async(&self, context: &'static Context, id: K, from: &'static [K], to: &'static [K]) -> BoxFuture<'static, Result<()>>;

	/// 获取已经就绪的依赖的数量
	fn load_ready_count(&self) -> usize;
	/// 增加已就绪的依赖的数量
	fn add_ready_count(&self, count: usize) -> usize;
	/// 增加已就绪的依赖的数量
	fn store_ready_count(&self, count: usize);
}

pub trait GetRunnble<K: 'static + ThreadSync, Context: 'static + ThreadSync, R: Runnble<K, Context>> {
	// type Runnble: Runnble<K, Context>;
	fn get_runnble(&self, id: K) -> Option<&R>;
}

impl<
K: Key + ThreadSync + 'static,
Context: 'static + ThreadSync,
R: ThreadSync + 'static + Runnble<K, Context>,
T: DirectedGraph<K, R> + ThreadSync + 'static,
> GetRunnble<K, Context, R> for T {

    fn get_runnble(&self, id: K) -> Option<&R> {
        self.get(id).map(|r| {r.value()})
    }
}

/// 异步图执行
pub async fn async_graph<
K: Key + ThreadSync + 'static,
V: ThreadSync + 'static,
Context: 'static + ThreadSync,
A: AsyncRuntime<()>,
R: Runnble<K, Context> + ThreadSync + 'static,
G: DirectedGraph<K, V> + GetRunnble<K, Context, R> + ThreadSync + 'static,
> (
	rt: A,
	graph: &G,
	context: &Context,
) -> Result<()> where <G as DirectedGraph<K, V>>::Node: Sync{

	let context = unsafe {transmute::<_, &'static Context>(context)};
	let graph = unsafe {transmute::<_, &'static G>(graph)};
	// 获得图的to节点的数量
	let mut count = graph.to_len();
	if count == 0 {
		return Ok(());
	}
	let (producor, consumer) = bounded(count);
	for k in graph.from() {
		let an = AsyncGraphNode::new(graph.clone(), k.clone(), producor.clone());
		let end_r = an.exec(rt.clone(), graph.get(*k).unwrap(), graph.get_runnble(*k).unwrap(), context);
		// 减去立即执行完毕的数量
		count -= end_r.unwrap();
	}
	// println!("wait count:{}", count);
	let r = AsyncGraphResult { count, consumer };
	r.reduce().await
}



/// 异步结果
pub struct AsyncGraphResult {
    count: usize,                      //派发的任务数量
    consumer: Receiver<Result<usize>>, //异步返回值接收器
}
/*
* 异步结果方法
*/
impl AsyncGraphResult {
    /// 归并所有派发的任务
    pub async fn reduce(mut self) -> Result<()> {
        loop {
            match self.consumer.recv_async().await {
                Err(e) => {
                    //接收错误，则立即返回
                    return Err(Error::new(
                        ErrorKind::Other,
                        format!("graph result failed, reason: {:?}", e),
                    ));
                }
                Ok(r) => match r {
                    Ok(count) => {
                        //接收成功，则检查是否全部任务都完毕
                        self.count -= count;
                        if self.count == 0 {
                            return Ok(());
                        }
                    }
                    Err(e) => {
                        return Err(Error::new(
                            ErrorKind::Other,
                            format!("graph node failed, reason: {:?}", e),
                        ))
                    }
                },
            }
        }
    }
}

/// 异步图节点执行
pub struct AsyncGraphNode<
    Context: 'static + ThreadSync,
    K: Key + ThreadSync + 'static,
    R: Runnble<K, Context> + 'static,
    G: DirectedGraph<K, V> + GetRunnble<K, Context, R> + ThreadSync + 'static,
	V: ThreadSync + 'static,
> {
    graph: &'static G,
    key: K,
    producor: Sender<Result<usize>>, //异步返回值生成器
    _k: PhantomData<(R, V)>,
    _c: PhantomData<Context>,
}

impl<
        Context: 'static + ThreadSync,
        K: Key + ThreadSync + 'static,
		V: ThreadSync + 'static,
        R: Runnble<K, Context>,
        G: DirectedGraph<K, V> + GetRunnble<K, Context, R> + ThreadSync + 'static,
    > AsyncGraphNode<Context, K, R, G, V>
{
    pub fn new(graph: &'static G, key: K, producor: Sender<Result<usize>>) -> Self {
        AsyncGraphNode {
            graph,
            key,
            producor,
            _k: PhantomData,
            _c: PhantomData,
        }
    }
}
unsafe impl<
        Context: 'static + ThreadSync,
        K: Key + ThreadSync + 'static,
		V: ThreadSync + 'static,
        R: Runnble<K, Context>,
        G: DirectedGraph<K, V> + GetRunnble<K, Context, R> + ThreadSync + 'static,
    > Send for AsyncGraphNode<Context, K, R, G, V>
{
}

impl<
        Context: 'static + ThreadSync,
        K: Key + ThreadSync + 'static,
		V: ThreadSync + 'static,
        R: Runnble<K, Context> + 'static,
        G: DirectedGraph<K, V> + GetRunnble<K, Context, R> + ThreadSync + 'static,
    > AsyncGraphNode<Context, K, R, G, V> where <G as DirectedGraph<K, V>>::Node: Sync
{
    /// 执行指定异步图节点到指定的运行时，并返回任务同步情况下的结束数量
    pub fn exec<A: AsyncRuntime<()>>(
        self,
        rt: A,
        node: &G::Node,
		runner: &R,
        context: &'static Context,
    ) -> Result<usize> {
        match runner.is_sync() {
            None => {
                // 该节点为空节点
                return self.exec_next(rt, node, context);
            }
            Some(true) => {
                // 同步节点
                let r = runner.get_sync();
				let node = unsafe{transmute::<_, &'static G::Node>(node)};
                rt.clone().spawn(async move {
                    // 执行同步任务
                    r.run(context, node.key().clone(), node.from(), node.to());

                    self.exec_async(rt, context).await;
                })?;
            }
            _ => {
				let (id, from, to) = (node.key().clone(), unsafe { transmute(node.from()) }, unsafe { transmute(node.to()) });
                let f = runner.get_async(context, id, from, to);
                rt.clone().spawn(async move {
                    // 执行异步任务
                    if let Err(e) = f.await {
                        let _ = self.producor.into_send_async(Err(e)).await;
                        return;
                    }
                    self.exec_async(rt, context).await;
                })?;
            }
        }
        Ok(0)
    }
    /// 递归的异步执行
    async fn exec_async<A: AsyncRuntime<()>>(self, rt: A, context: &'static Context) {
        // 获取同步执行exec_next的结果， 为了不让node引用穿过await，显示声明它的生命周期
        let r = {
            let node = self.graph.get(self.key).unwrap();
            self.exec_next(rt, node, context)
        };
        if let Ok(0) = r {
            return;
        }
        let _ = self.producor.into_send_async(r).await;
    }

    /// 递归的同步执行
    fn exec_next<A: AsyncRuntime<()>>(
        &self,
        rt: A,
        node: &G::Node,
        context: &'static Context,
    ) -> Result<usize> {
        // 没有后续的节点，则返回结束的数量1
        if node.to_len() == 0 {
            return Ok(1);
        }
        let mut sync_count = 0; // 记录同步返回结束的数量
        for k in node.to() {
			let n = self.graph.get(*k).unwrap();
			let r = self.graph.get_runnble(*k).unwrap();
            
			let count = r.add_ready_count(1) + 1;
			// println!("node: {:?}, count: {} from: {}", n.key(), count, n.from_len());
            // 将所有的to节点的计数加1，如果计数为from_len， 则表示全部的依赖都就绪
            if count != n.from_len() {
                //println!("node1: {:?}, count: {} ", n.key(), n.load_count());
                continue;
            }
            // 将状态置为0，创建新的AsyncGraphNode并执行
            r.store_ready_count(0);

            let an = AsyncGraphNode::new(self.graph, k.clone(), self.producor.clone());

            sync_count += an.exec(rt.clone(), n, r, context)?;
        }

        Ok(sync_count)
    }
}

pub trait RunFactory<K: 'static + ThreadSync, Context: 'static + ThreadSync> {
    type R: Runner<K, Context>;
    fn create(&self) -> Self::R;
}

pub trait AsyncNode<K: Key + 'static + ThreadSync, Context: 'static + ThreadSync>:
    Fn(&'static Context,  K, &'static[K], &'static[K]) -> BoxFuture<'static, Result<()>> + ThreadSync + 'static
{
}

impl<
		K: Key + 'static + ThreadSync, 
        Context: 'static + ThreadSync,
        T: Fn(&'static Context, K, &'static[K], &'static[K]) -> BoxFuture<'static, Result<()>> + ThreadSync + 'static,
    > AsyncNode<K, Context> for T
{
}

pub enum ExecNodeInner<
	K: 'static + ThreadSync,
    Context: 'static + ThreadSync,
    Run: Runner<K, Context>,
    Fac: RunFactory<K, Context, R = Run>,
> {
    None,
    Sync(Fac),
    Async(Box<dyn AsyncNode<K, Context>>, PhantomData<K>),
}

pub struct ExecNode<
	K: 'static + ThreadSync,
    Context: 'static + ThreadSync,
    Run: Runner<K, Context>,
    Fac: RunFactory<K, Context, R = Run>,
> {
    exec: ExecNodeInner<K, Context, Run, Fac>,
	read_count: AtomicUsize,
}

impl<
K: 'static + ThreadSync,
Context: 'static + ThreadSync,
Run: Runner<K, Context>,
Fac: RunFactory<K, Context, R = Run>,
> ExecNode<K, Context, Run, Fac>  {
	pub fn new_sync(v: Fac) -> Self {
		Self {
			exec: ExecNodeInner::Sync(v),
			read_count: AtomicUsize::new(0),
		}
	}

	pub fn new_async(v: Box<dyn AsyncNode<K, Context>>) -> Self {
		Self {
			exec: ExecNodeInner::Async(v, PhantomData),
			read_count: AtomicUsize::new(0),
		}
	}

	pub fn new_none() -> Self {
		Self {
			exec: ExecNodeInner::None,
			read_count: AtomicUsize::new(0),
		}
	}
}

impl<
		K: 'static + ThreadSync,
        Context: 'static + ThreadSync,
        Run: Runner<K, Context> + ThreadSync + 'static,
        Fac: RunFactory<K, Context, R = Run>,
    > Runnble<K, Context> for ExecNode<K, Context, Run, Fac>
{
    type R = Run;

    fn is_sync(&self) -> Option<bool> {
        match self.exec {
            ExecNodeInner::None => None,
            ExecNodeInner::Sync(_) => Some(true),
            _ => Some(false),
        }
    }
    /// 获得需要执行的同步函数
    fn get_sync(&self) -> Self::R {
        match &self.exec {
            ExecNodeInner::Sync(r) => r.create(),
            _ => panic!(),
        }
    }
    /// 获得需要执行的异步块
    fn get_async(&self, context: &'static Context, id: K, from: &'static [K], to: &'static[K]) -> BoxFuture<'static, Result<()>> {
        match &self.exec {
            ExecNodeInner::Async(f, _) => f(context, id, from, to),
            _ => panic!(),
        }
    }

    fn load_ready_count(&self) -> usize {
        self.read_count.load(Ordering::Relaxed)
    }

    fn add_ready_count(&self, count: usize) -> usize {
        self.read_count.fetch_add(count, Ordering::SeqCst)
    }

    fn store_ready_count(&self, count: usize) {
        self.read_count.store(count, Ordering::SeqCst);
    }
}

#[test]
fn test_graph() {
    use futures::FutureExt;
    use pi_async_rt::prelude::multi_thread::{MultiTaskRuntimeBuilder, StealableTaskPool};
    use pi_graph::NGraphBuilder;
    use std::time::Duration;
	use pi_slotmap::{DefaultKey, SlotMap};

    struct A(usize);

    impl Runner<DefaultKey, ()> for A {
        fn run(self, _: &'static (), _id: DefaultKey, _from: &[DefaultKey], _to: &[DefaultKey]) {
            println!("A id:{}", self.0);
        }
    }

    struct B(usize);
    impl RunFactory<DefaultKey, ()> for B {
        type R = A;
        fn create(&self) -> A {
            A(self.0)
        }
    }
    fn syn(id: usize) -> ExecNode<DefaultKey, (), A, B> {
        ExecNode::new_sync(B(id))
    }
    fn asyn(id: usize) -> ExecNode<DefaultKey, (), A, B> {
        let f = move |_empty, _, _, _| -> BoxFuture<'static, Result<()>> {
            async move {
                println!("async id:{}", id);
                Ok(())
            }
            .boxed()
        };
        ExecNode::new_async(Box::new(f))
    }

    let pool = MultiTaskRuntimeBuilder::<(), StealableTaskPool<()>>::default();
    let rt0 = pool.build();
    let rt1 = rt0.clone();
	let mut map = SlotMap::<DefaultKey, ()>::default();
	let nodes = vec![
		map.insert(()), map.insert(()), map.insert(()), map.insert(()), 
		map.insert(()), map.insert(()), map.insert(()), map.insert(()), 
		map.insert(()), map.insert(()), map.insert(()), map.insert(()), 
	];

    let mut graph = NGraphBuilder::new();
	graph.node(nodes[1], asyn(1))
        .node(nodes[2], asyn(2))
        .node(nodes[3], syn(3))
        .node(nodes[4], asyn(4))
        .node(nodes[5], asyn(5))
        .node(nodes[6], asyn(6))
        .node(nodes[7], asyn(7))
        .node(nodes[8], asyn(8))
        .node(nodes[9], asyn(9))
        .node(nodes[10], ExecNode::new_none())
        .node(nodes[11], asyn(11))
        .edge(nodes[1],nodes[4] )
        .edge(nodes[2],nodes[4] )
        .edge(nodes[2],nodes[5] )
        .edge(nodes[3],nodes[5] )
        .edge(nodes[4],nodes[6] )
        .edge(nodes[4],nodes[7] )
        .edge(nodes[5],nodes[8] )
        .edge(nodes[9],nodes[10] )
        .edge(nodes[10],nodes[11] );
	let graph = graph.build()
        .unwrap();
	
    let _ = rt0.spawn(async move {
        let _: _ = async_graph(rt1, &graph, &()).await;
        println!("ok");
    });
    std::thread::sleep(Duration::from_millis(5000));
}

#[test]
fn test() {
    use pi_async_rt::prelude::multi_thread::{MultiTaskRuntimeBuilder, StealableTaskPool};
    use std::time::Duration;

    let pool = MultiTaskRuntimeBuilder::<(), StealableTaskPool<()>>::default();
    let rt0 = pool.build();
    let rt1 = rt0.clone();
    let _ = rt0.spawn(async move {
        let mut map_reduce = rt1.map_reduce(10);
        let rt2 = rt1.clone();
        let rt3 = rt1.clone();
        let _ = map_reduce.map(rt1.clone(), async move {
            rt1.timeout(300).await;
            println!("1111");
            Ok(1)
        });

        let _ = map_reduce.map(rt2.clone(), async move {
            rt2.timeout(1000).await;
            println!("2222");
            Ok(2)
        });
        let _ = map_reduce.map(rt3.clone(), async move {
            rt3.timeout(600).await;
            println!("3333");
            Ok(3)
        });
        for r in map_reduce.reduce(true).await.unwrap() {
            println!("r: {:?}", r);
        }
    });
    std::thread::sleep(Duration::from_millis(5000));
}
